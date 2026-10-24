# Armed-live (Task 8.5 R36)

The armed lane sends its arms as validity transactions: each waits in Base's
builder for its trigger and costs nothing unless it lands. It runs inside
`apex canary`, from 16 senders of its own (one per slot), never the trader.
The design is `docs/superpowers/specs/2026-10-23-armed-lane-v2-design.md`;
the plan, `docs/superpowers/plans/2026-10-23-armed-lane-v2.md`.

Every cap is in `ops/canary.base.yaml`'s `armed:` section, which the engine
and the live worker both read: the daily loss budget (0.01 ETH, realized
losses plus the worst case of everything pending), the tip cap, the size
cap, the L1 fee bound and each sender's reserve. `armed.live:` says only where
things are.

Paths below are the host's. `#` is root on the host, `$` the operator's
machine.

Armed-live's own files are in `/opt/apex/var/armed/`: the senders' keys,
the live ledger, the census's targets and the kill file.
- It is the only place the unit, which runs as `apex` under
  `ProtectSystem=strict`, can both read and write.
- A new revision's archive of `/opt/apex/var/apex` (README §7) leaves it in
  place.

## 1. Before the first live boot, in this order

### a. The senders' keys (the host)

```
# install -d -o apex -g apex -m 0700 /opt/apex/var/armed
# sudo -u apex /opt/apex/bin/apex keys new --count 16 --out /opt/apex/var/armed/armed.keys
```

It prints the 16 addresses and nothing else.
- The file is created 0400, owned by `apex`, and never overwritten: a second
  run refuses.
- `apex canary` refuses a keys file anyone but its owner can read, and one
  whose count is not `armed.max_arms`.
- Back it up the way `/etc/apex/shadow.env` is backed up. Never copy it off
  the host.

### b. `ops/armed-senders.json` (the operator)

Copy `ops/armed-senders.example.json` to `ops/armed-senders.json` and put the
16 printed addresses in `senders`. Addresses only: no key goes in it.
`amount_wei` is what each sender is topped up to (0.002 ETH in the pilot).

### c. The owner's two transactions, then their check

Both are signed by the owner (`0x69D5…8A3`), typed at the prompt. Dry-run each
first; the dry run reads Base through the public endpoint and sends nothing.

```
$ forge clean
$ forge script script/AuthorizeArmedSenders.s.sol --rpc-url https://mainnet.base.org --sender 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3
$ forge script script/AuthorizeArmedSenders.s.sol --rpc-url https://mainnet.base.org --sender 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3 --broadcast --interactive
$ forge script script/FundArmedSenders.s.sol --rpc-url https://mainnet.base.org --sender 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3
$ forge script script/FundArmedSenders.s.sol --rpc-url https://mainnet.base.org --sender 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3 --broadcast --interactive
```

- Ignore the 21 "failed to parse" warnings: they are known and harmless.
- **Authorize** calls `setExecutor(sender, true)` on the lean executor for
  each sender, then checks `executors(sender)` reads true for every one. The
  dry run on 2026-10-24, with the example file's two throwaway senders,
  showed 2 calls and 127,046 gas for the script.
- **Fund** tops each sender up to `amount_wei`. A sender already at or over
  it is sent nothing, so a second run refills rather than doubles. The owner
  needs 16 × 0.002 = 0.032 ETH, plus gas, on Base. On 2026-10-24 it held
  0.000119 ETH, so the dry run stopped at the first transfer (`OutOfFunds`).
  `test/ArmedSenders.t.sol` covers the top-up.
- **Revocation:** set `"allowed": false` and run Authorize again.
  - Armed-live reads `executors(sender)` and the executor's circuit on every
    head, and stops sending from a revoked sender at the next one
    (`skipped.revoked`).
  - A transaction already in the builder can still land in its window (up
    to 10 s), revert at `NotTrader` and pay gas. Touch the kill file first,
    and revoke once the window has passed.

### d. The API probe (the host)

Run it once, with the armed lane in shadow, on a sender armed-live's ledger
has no record of. Its nonce moves, and the boot trips on a sender whose
nonce the ledger did not leave there; the probe refuses a sender the ledger
knows.

```
# sudo -u apex bash -c 'set -a; . /etc/apex/shadow.env; set +a; cd /opt/apex && bin/apex validity-probe --config ops/canary.base.yaml --sender 15'
```

It prints one JSON line per case, each ending in `"pass": true` or `false`;
the exit status is 0 only if all six pass. The cases:

1. A predicate that can never hold expires.
2. One already true lands within two blocks.
3. One held past a same-nonce replacement at the same fees expires. The
   replacement's answer and `base_transactionStatus` are printed.
4. The nonce is sent again after that bound has passed.
5. An arm's eight predicates, all true, land.
6. Five back-to-back bumps of an eighth each expire.

It costs about two 21,000-gas transactions from sender 15.

### e. Gate A′: one day of armed-shadow on the targets

With `armed.mode: shadow` and `armed.targets` set, run at least a day. The
gate passes when all four hold:

- quote and simulation agree on ≥ 95% of fires (`ops/host/remote.sh armed`);
- every armed pool's words matched the book (no "never armed" boot warnings);
- the probe passed;
- BlockPI RU stayed under budget.

### f. Going live

Set `armed.mode: live` in `ops/canary.base.yaml`, deploy it (README §7), and
restart:

```
# systemctl restart apex-canary
# journalctl -u apex-canary --no-pager | grep -E "armed-live sends|armed-live's ledger is read"
```

The boot refuses, naming the cause, when any of these holds:

- the keys file holds a key count other than 16;
- the executor does not list a sender in `executors(address)`;
- the targets are missing, stale or not `ok`;
- `armed.live` is absent.

Nothing is sent until three things are done:

- every transaction the ledger left pending is settled (landed or expired);
- every sender's nonce is read;
- twenty heads have shown the configured cadence (about 40 s).

## 2. Running

### Reading it

```
$ ops/host/remote.sh live
```

That prints the report's `armed` section. `armed.live` carries:

- **Counts:** `submitted`, `accepted`, `refused`, `silent` (may have been
  sent), `skipped` by reason, `landed`, and `won` (landed and paid).
- **Money:** `gross_wei`, `cost_wei`, `net_today_wei`; `reserved_wei` (the
  worst case of everything pending) against `budget_wei`.
- **State:** `halted`, the measured and configured cadence, and
  `expiry_horizon_blocks` (the furthest pending bound past the last head).
- **`senders`:** each sender's balance, nonce and pending worst case, and
  `days_left` at the last day's spend.

`skipped.balance` means a sender cannot pay its worst case and keep its
reserve: run Fund again. `skipped.size` counts arms over `max_size_wei`.
`armed.live_dropped` counts engine passes that found the send queue full.

### Stopping it

- **Soft:** `touch /opt/apex/var/armed/armed.disable`. Nothing new is sent
  while the file exists; pending transactions still settle. Remove the file
  to resume. It is not a halt.
- **By itself:** nothing is sent while the owner's stops were last read more
  than three block times ago, which is what a frozen head feed looks like.
- **Halts** are written to `/opt/apex/var/armed/armed-live.jsonl` and survive
  a restart. `halted` names the reason.
  - *The loss cap:* the day's realized loss reached the budget. It ends at
    the next UTC midnight, and a reset does not clear it.
  - *The tripwire:* a sender's nonce moved with no receipt among our
    transactions, moved by more than one, or a receipt did not read.
    - The nonces it cannot account for are written off at their worst case,
      which counts against the day's budget (`abandoned` in the ledger).
    - It holds until reset, after you have found what moved the nonce. A
      reset takes every idle sender at the chain's nonce from then on:

```
# systemctl stop apex-canary
# sudo -u apex bash -c 'printf "{\"kind\":\"reset\",\"at_ms\":%s}\n" "$(date +%s%3N)" >> /opt/apex/var/armed/armed-live.jsonl'
# systemctl start apex-canary
```

- **Hard, the owner's own, on chain:** the executor's circuit
  (`tripCircuit()`), or the revocation in §1c.
  - Either stops every send from the next head.
  - Touch the kill file first: a transaction already waiting can still land
    in its window, revert and pay gas.
- **Off:** set `armed.mode: shadow` and restart.

### The live ledger

`armed-live.jsonl` is the record of every send, and the boot replays it.

- Never move, truncate or edit it while armed-live is on, except to append a
  reset. Without it, the boot cannot settle what was pending, and a landing
  would go unaccounted against the day's budget.
- It lives in `var/armed`, so README §7's archive of `var/apex` leaves it
  where it is.
- It grows by about 100 MB a day at the full renewal rate. The boot compacts
  it once it passes 10,000 records:
  - every landing, abandonment, halt and reset stays, with every record a
    pending transaction or a nonce view needs;
  - skips and settled renewals go;
  - the original is kept beside it as `armed-live.<ms>.jsonl`;
  - the compaction is used only if it replays to the same state.
- The daily restart therefore keeps it to about a day's worth.

### The daily census

`apex-targets.timer` runs `arb_census.targets` once a day. It writes
`/opt/apex/var/armed/targets.json` and restarts `apex-canary`, so the new
targets load.

A file that is not fit to arm (`ok: false`) goes beside it as
`targets.unfit.json`, never over the last good one. The job then fails, so
the canary is not restarted into a live boot that would refuse it.

- Each restart settles the pending transactions and waits for the cadence
  again: about 40 s without sends.
- The census reads the live ledger for "ours vs the market".
- It reads the flashblock ledgers in `var/apex`. A new revision's archive
  moves those, so until a day passes it falls back to block offsets.
- Targets older than `targets_max_age_s` (2 days) stop a live boot.

### The Denim day

Denim (Base mainnet, ~November 2026) makes blocks 200 ms and ends
`pendingLogs` and flashblocks. On that day, make three edits in both
`ops/canary.base.yaml` and `ops/shadow.base.yaml`, then restart:

- add `feed:` with `pending_logs: false`, so the feed is heads and canonical logs;
- remove the `flashblocks:` section, so no stream is opened and no ledger written;
- set `armed.block_time_ms: 200`.

W stays 10 s, now 50 blocks, inside Base's 60 s bound limit.

Until the configuration matches, the cadence guard stops every send by
itself: the measured cadence would be 200 ms against a configured 2,000.
