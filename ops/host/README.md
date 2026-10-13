# The co-located shadow host (Task 8.5 R25)

The shadow, run next to Base's builder instead of from New Zealand, with the
race and flashblock ledgers on (`docs/superpowers/specs/2026-10-11-colocated-shadow-design.md`).
Nothing on this host broadcasts: the shadow's only lane is the null dispatcher.

| File | What |
|---|---|
| `apex-shadow.service` | the systemd unit |
| `provision.sh` | one-time host setup, as root |
| `deploy.sh` | packages a committed revision and the pool inventory (run on the operator's machine) |
| `shadow.env.example` | the two secret names `/etc/apex/shadow.env` holds |

Conventions below: `$` lines run on the operator's machine, `#` lines in an SSM
session on the instance after `sudo -i`. No secret is ever typed on a command
line: keys go into root-only files through an editor.

## 1. AWS setup (operator, once)

An instance role for SSM, a security group with **no inbound rules** and
outbound 443/80 only. No SSH key pair. DNS (the VPC resolver) and Amazon Time
Sync (169.254.169.123) are link-local and need no rule.

```
$ export AWS_REGION=us-east-1        # repeat the security group for us-east-2 for the probe
$ aws iam create-role --role-name apex-shadow-ssm --assume-role-policy-document '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}'
$ aws iam attach-role-policy --role-name apex-shadow-ssm --policy-arn arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore
$ aws iam create-instance-profile --instance-profile-name apex-shadow-ssm
$ aws iam add-role-to-instance-profile --instance-profile-name apex-shadow-ssm --role-name apex-shadow-ssm
$ VPC=$(aws ec2 describe-vpcs --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)
$ SG=$(aws ec2 create-security-group --group-name apex-shadow --description "apex shadow: no inbound" --vpc-id $VPC --query GroupId --output text)
$ aws ec2 revoke-security-group-egress --group-id $SG --ip-permissions '[{"IpProtocol":"-1","IpRanges":[{"CidrIp":"0.0.0.0/0"}]}]'
$ aws ec2 authorize-security-group-egress --group-id $SG --ip-permissions '[{"IpProtocol":"tcp","FromPort":443,"ToPort":443,"IpRanges":[{"CidrIp":"0.0.0.0/0"}]},{"IpProtocol":"tcp","FromPort":80,"ToPort":80,"IpRanges":[{"CidrIp":"0.0.0.0/0"}]}]'
```

Launching an instance (`c7i.large` for a probe, `c7i.xlarge` for the host):
Ubuntu 24.04 LTS, IMDSv2 required, an encrypted 50 GB gp3 root volume.

```
$ aws ec2 run-instances --instance-type c7i.xlarge \
    --image-id resolve:ssm:/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id \
    --iam-instance-profile Name=apex-shadow-ssm --security-group-ids $SG \
    --metadata-options HttpTokens=required,HttpEndpoint=enabled \
    --block-device-mappings 'DeviceName=/dev/sda1,Ebs={VolumeSize=50,VolumeType=gp3,Encrypted=true,DeleteOnTermination=true}' \
    --tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=apex-shadow}]' \
    --query 'Instances[0].InstanceId' --output text
$ aws ssm start-session --target <instance-id>
```

## 2. Packaging and transfer

The host has no inbound path, so the archives go through a private S3 bucket
and short-lived presigned URLs (the instance role needs no S3 permission).

```
$ ops/host/deploy.sh <revision>          # prints the manifest's sha256: keep it
$ BUCKET=apex-shadow-dist-$(aws sts get-caller-identity --query Account --output text)
$ aws s3 mb s3://$BUCKET                 # once; new buckets block public access
$ aws s3 cp dist/ s3://$BUCKET/$(cut -c1-12 dist/REVISION)/ --recursive
$ for f in $(cd dist && ls); do echo "$f $(aws s3 presign s3://$BUCKET/$(cut -c1-12 dist/REVISION)/$f --expires-in 3600)"; done
```

On the instance, for each printed line (`curl` with the URL in quotes):

```
# mkdir -p /root/dist && cd /root/dist
# curl -fsS -o <file> '<presigned url>'
# sha256sum MANIFEST.sha256              # must equal the value deploy.sh printed
# sha256sum -c MANIFEST.sha256           # every line OK
# tar -xzf apex-*.tar.gz -C /root        # for provision.sh
```

## 3. The region probe (once per candidate: us-east-1, us-east-2)

Run both candidates in the same hour, on `c7i.large` instances.

```
# bash /root/apex/ops/host/provision.sh
# tar -xzf /root/dist/apex-*.tar.gz -C /opt && chown -R apex:apex /opt/apex
# sudo -u apex -H bash -c 'cd /opt/apex && CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 ~/.cargo/bin/cargo build -j 2 --release --locked -p apex-runtime --bin apex'
# install -m 0600 /dev/null /root/probe.env && nano /root/probe.env    # one line: APEX_SECRET_BLOCKPI_KEY=<BLOCKPI_KEY from .env>
# (set -a; . /root/probe.env; set +a; /opt/apex/target/release/apex probe --minutes 60) | tee /root/probe-$(cloud-init query region).json
```

Choose by the spec's §1 rule:

- the lower `arrival_offset_ms.p50` wins;
- within 5 ms of each other, the lower `blockpi_delay_ms.p90` wins;
- δ for the analyzer is half the winner's `sequencer` `p50_ms`.

Reference, from Wellington on 2026-10-12 (5 minutes): arrival offset p50 137 ms,
BlockPI delay p50 22 / p90 95 ms, sequencer round trip 219 ms.

Record both JSON reports, then terminate both probe instances
(`aws ec2 terminate-instances --instance-ids <id>`; the volume goes with them).

## 4. Install (the host, `c7i.xlarge` in the chosen region)

Before the first start, do §6's dry run and check that the laptop shadow is
**not** running: the two would share the trader key.

```
# bash /root/apex/ops/host/provision.sh
# tar -xzf /root/dist/apex-*.tar.gz -C /opt
# tar -xzf /root/dist/data-base.tar.gz -C /opt/apex
# chown -R apex:apex /opt/apex && chown root:root /opt/apex/bin
# sudo -u apex -H bash -c 'cd /opt/apex && ~/.cargo/bin/cargo build -j 4 --release --locked -p apex-runtime --bin apex'
# install -o root -g root -m 0755 /opt/apex/target/release/apex /opt/apex/bin/apex
# install -o root -g root -m 0644 /root/dist/REVISION /opt/apex/REVISION
# install -o root -g root -m 0600 /dev/null /etc/apex/shadow.env && nano /etc/apex/shadow.env   # the two lines of shadow.env.example, with values
# install -m 0644 /opt/apex/ops/host/apex-shadow.service /etc/systemd/system/apex-shadow.service
# systemctl daemon-reload && systemctl enable --now apex-shadow
```

The run's fourteen days start at this first start: note the time
(`systemctl show -p ActiveEnterTimestamp apex-shadow`).

## 5. Checks (after install, and after any change)

```
# systemctl status apex-shadow --no-pager                 # active (running)
# cd /opt/apex && scripts/shadow-status.sh                # after the first report (5 min): flashblocks, race, signer lines
# kill -9 $(systemctl show -p MainPID --value apex-shadow); sleep 15; systemctl is-active apex-shadow   # active: restarted
# reboot                                                   # then, in a new session: systemctl is-active apex-shadow
# stat -c '%a %U:%G' /etc/apex/shadow.env                  # 600 root:root
# ss -ltnp                                                 # nothing listening but the SSM agent's local sockets
# chronyc tracking | grep -E 'Reference ID|System time'    # 169.254.169.123, offset well under 1 ms
```

`systemctl stop apex-shadow` drains (the log's last lines say every worker
stopped) and writes a final report.

## 6. Revocation of the trader key

The trader key `0xCB436Ba3acb945b3fc8EE6345857262584356595` is on this host.
A shadow never sends, so its nonce never moves; if it does, the shadow logs
`the signer's nonce moved` at ERROR on every head, the report carries
`signer.nonce_moved: true`, and `shadow-status.sh` prints
`SIGNER NONCE MOVED`. Then revoke at once.

The owner (`0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3`) calls the
BatchRouter (`0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2`), which owns the
executor clone (`0x8940B565D050974b2b589B70De43bCb753b1F93B`), with
`multicall([clone], [setExecutor(trader, false)])`:

```
$ INNER=$(cast calldata "setExecutor(address,bool)" 0xCB436Ba3acb945b3fc8EE6345857262584356595 false)
$ OUTER=$(cast calldata "multicall(address[],bytes[])" "[0x8940B565D050974b2b589B70De43bCb753b1F93B]" "[$INNER]")
$ echo $OUTER
0x63fb0b960000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000010000000000000000000000008940b565d050974b2b589b70de43bcb753b1f93b0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000441e1bff3f000000000000000000000000cb436ba3acb945b3fc8ee6345857262584356595000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000
```

Dry run before the host starts (`BLOCKPI_KEY` exported from `.env`, so
foundry.toml's `base` endpoint resolves it; never on the command line):

```
$ cast call 0x8940B565D050974b2b589B70De43bCb753b1F93B "executors(address)(bool)" 0xCB436Ba3acb945b3fc8EE6345857262584356595 --rpc-url base   # true
$ cast call --from 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3 0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2 $OUTER --rpc-url base               # 0x
$ cast call --from 0xCB436Ba3acb945b3fc8EE6345857262584356595 0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2 $OUTER --rpc-url base               # reverts NotOwner() (0x30cd7471)
$ cast estimate --from 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3 0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2 $OUTER --rpc-url base
```

All four were run on 2026-10-12: `true`, `0x`, `NotOwner()`, 43,839 gas.

If the tripwire fires, the operator broadcasts with the owner's key, typed at
the prompt:

```
$ cast send 0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2 $OUTER --rpc-url base --interactive
$ cast call 0x8940B565D050974b2b589B70De43bCb753b1F93B "executors(address)(bool)" 0xCB436Ba3acb945b3fc8EE6345857262584356595 --rpc-url base   # false
```

Then stop the host (`systemctl stop apex-shadow`) and treat the instance as
compromised: terminate it. A new trader needs its own `setExecutor(new, true)`
and a new `signer.address` in `ops/shadow.base.yaml`.

## 7. Updating

```
$ ops/host/deploy.sh <revision>     # then §2's transfer and checks
# tar -xzf /root/dist/apex-*.tar.gz -C /opt && chown -R apex:apex /opt/apex && chown root:root /opt/apex/bin
# sudo -u apex -H bash -c 'cd /opt/apex && ~/.cargo/bin/cargo build -j 4 --release --locked -p apex-runtime --bin apex'
# install -o root -g root -m 0755 /opt/apex/target/release/apex /opt/apex/bin/apex
# install -o root -g root -m 0644 /root/dist/REVISION /opt/apex/REVISION
# systemctl restart apex-shadow
```

The same revision restarted is the same run. A new revision starts a new
fourteen-day clock: archive `/opt/apex/var/apex` first
(`systemctl stop apex-shadow && mv /opt/apex/var/apex /opt/apex/var/run-<n> && install -d -o apex -g apex /opt/apex/var/apex`).

## 8. The contest analysis

On the host, with only the BlockPI key in the analyzer's environment:

```
# cd /opt/apex/scripts/data
# APEX_SECRET_BLOCKPI_KEY="$(sed -n 's/^APEX_SECRET_BLOCKPI_KEY=//p' /etc/apex/shadow.env)" \
    python3 race_analysis.py --dir /opt/apex/var/apex --delta-ms <δ from §3> --out /root/race_rows.jsonl
```

It prints G1 and G2 (`g1_pass`, `g2_pass`), the share of contested gaps we
would have landed ahead of the rival (`ahead_share`), and the rivals' offsets
from the trigger's flashblock.

## 9. Armed-shadow (R26)

The shadow's `armed` worker arms the best routes' gaps pass after pass, the way
validity transactions would wait in Base's builder: built and recorded, never
sent (`var/apex/armed-YYYY-MM-DD.jsonl`). `apex armed-eval` judges each arm
once its window has closed. It asks whether the arm would have fired, whether
it would have won against the rival and at which bid, and what it would have
netted (`var/apex/armed-verdicts-YYYY-MM-DD.jsonl`).

Install the timer (once, after the binary from §4 or §7):

```
# install -m 0644 /opt/apex/ops/host/apex-armed-eval.service /opt/apex/ops/host/apex-armed-eval.timer /etc/systemd/system/
# systemctl daemon-reload && systemctl enable --now apex-armed-eval.timer
```

Checks:

```
# systemctl list-timers apex-armed-eval.timer       # next run within 5 minutes
# journalctl -u apex-armed-eval -n 20 --no-pager     # each run prints {"judged":…,"fired":…,"cursor":…}
$ ops/host/remote.sh armed                           # the bid curve so far
$ ops/host/remote.sh                                 # the "armed" and "moves" lines
```

**Reading the summary.** It is keyed by UTC day, then by bid fraction `f`
in per mille (50 = 5% of the arm's expected surplus bid as priority fee):

- `fires`: arms whose predicates all held at some transaction in their
  window. A higher rung of the same slot firing on the same transaction
  counts once.
- `wins_optimistic` and `wins_pessimistic`: won if we landed in the
  trigger's flashblock, or in the next one.
- `net_usd_*`: what the wins netted after gas at the block's base fee, the
  bid and the L1 fee. Intermediate residue is not counted.
- `included_reverts`: fires that would have landed and reverted; their fee
  is a cost.
- `simulated` and `sim_agreed`: fires checked with `eth_simulateV1`, and how
  many agreed with the exact quote to the wei.
  - The simulation replays the whole fire block so far, then the transaction
    that would execute in that block, a renewal's own after one.
  - Before R26.1 it replayed only the pool-touching transactions, and a
    disagreement meant nothing. Since then, one is a finding: re-run it by
    hand before believing either side.
- `unfired_trigger_unmet` and `unfired_band_missed`: arms that never fired.
  Either the trigger pool never moved far enough, or it did while the other
  pool was outside its band.
  - Mostly unmet: the rungs are further out than the market moves in a
    window.
  - Mostly band missed: the bands are too narrow, or the arms are too old
    for the pools they watch.

**Gate A** (spec §6) needs all of these before the live pilot:
- at least 3 days of armed-shadow and at least 30 fires;
- a positive pessimistic net per day at some `f`;
- the simulation agreeing on at least 95% of checked fires;
- the operator's approval.

The status lines:
- `armed`: arms, renewals, armable slots, unarmable pools (boot layout
  check), the engine's last pass in ms;
- `armed pass` (R27.1): how the engine keeps up, averaged over the run. A slot
  is a cycle with one of its pools as the trigger. A slot whose pool changed
  is not armed until it is repriced, so the share "priced at the book as it
  stood" is the share a sender could take, and "left waiting a pass" is the
  backlog. A pass prices on all but one of the machine's cores for 150 ms,
  changed slots first, and the next follows at once while any slot waits
  (50 ms later when none does). A low share with slots waiting means the
  pass is saturated: fewer slots, fewer rungs or more cores;
- `lenders` (R27): what each flash lender holds of WETH. A trade borrows from
  Balancer while its holding covers the size, and from the Morpho lender above.
  Arms past Balancer's holding are labelled `"lender": "morpho"` in
  `armed-*.jsonl`. "morpho not configured" means `lenders.morpho` is absent from
  the config.
- `moves`: every applied Swap checked against the move model.
  - **Wrong ticks are the model's error and should stay near zero.**
  - Liquidity changes are mints and burns the book had not applied yet. On
    Base's Slipstream and Uniswap pools, liquidity is added and removed
    around swaps, so expect 7–13% (the 2026-10-12 smoke runs).

