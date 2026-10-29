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
  dry run on 2026-10-25, with the 16 senders made on the host, showed 16
  calls, every sender reading true afterwards.
- **Fund** tops each sender up to `amount_wei`. A sender already at or over
  it is sent nothing, so a second run refills rather than doubles. The owner
  needs 16 × 0.002 = 0.032 ETH, plus gas, on Base. The dry run on
  2026-10-25 showed 16 transfers, 0.032 ETH in all.
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
# bash -c 'set -a; . /etc/apex/shadow.env; set +a; cd /opt/apex && bin/apex validity-probe --config ops/canary.base.yaml --sender 15'
```

As root: `/etc/apex` is root's alone, and root reads the 0400 keys file.
The environment file is sourced into the process, never printed.

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

**The run on 2026-10-25** (sender 15, from the host) passed all six cases.
The sequencer answered every send in 12–24 ms. Three findings for live:
- *Replacement:* a held transaction is replaced only at higher fees
  (case 3's same-fee replacement: `replacement transaction underpriced`).
  The worker's eighth-higher bumps were all accepted (case 6).
- *After its bound,* a transaction's nonce takes the same fees again
  (case 4). A renewal begins where the engine's window ends, so it normally
  needs no bump.
- *An expired hash* can be sent again with new predicates, and lands when
  they hold (cases 2 and 5 re-sent identical bytes). Armed-live never
  repeats a hash: each renewal carries a fresh deadline.

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

## 3. R37: the second executor (`LeanExecutor2`)

`LeanExecutor2` adds a start token (WETH or USDC), Uniswap v4 hops, and
PoolManager's flash accounting as a lender. The design is
`docs/superpowers/specs/2026-10-28-v4-usdc-armed-lane-design.md`. It is
deployed once, by the owner. Then every arm uses it, and the current
executor keeps only the halted reactive lane.

Nothing here happens before gate A″ passes and the operator approves the
deploy (the spec's phase 5). The order: the v4 inventory (§3e) and the
pre-deploy shadow (§3f), gate A″ (§3g), the operator's decisions 2–4 (§3h,
made 2026-10-29), then §3a–c.

### a. Deploy (the owner)

```
$ forge clean
$ forge script script/DeployLeanExecutor2.s.sol --sig "run(address,address)" 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3 0xCB436Ba3acb945b3fc8EE6345857262584356595 --rpc-url https://mainnet.base.org --sender 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3
$ (the same, with --broadcast --interactive)
```

- It prints the address and the runtime code hash, which become
  `armed.executor2.address` and `.code_hash`.
- The script refuses another chain. It checks the roles and Morpho's USDC
  approval before anything is broadcast.
- The dry run on 2026-10-29 simulated about 2.53M gas, roughly 0.000027 ETH,
  and code hash `0xb67e7dd2…c7e`. A rebuilt contract changes the hash: use
  the broadcast's own.

### b. Authorize the 16 senders on it (the owner)

Set `executor` in `ops/armed-senders.json` to the new address. Then dry-run
and broadcast `AuthorizeArmedSenders.s.sol` exactly as in §1c, and check that
every sender reads true. The senders are the same 16, so no new keys are
needed. They stay at 0.002 ETH each (decision 2, §3h).

### c. Point the runtime at it (the host)

- In both `ops/canary.base.yaml` and `ops/shadow.base.yaml`:
  - set `armed.executor2.address` and `armed.executor2.code_hash`;
  - remove `armed.executor2.runtime_code`, the pre-deploy simulation's state
    override.
- Deploy those (README §7) and restart.
- The boot checks the code hash. A mismatch arms nothing.

### d. Stopping it

As in §2:
- touch the kill file first;
- trip the circuit or revoke on `LeanExecutor2` itself;
- revoke on the old executor too, if a sender is still allowed there.

### e. The v4 inventory: one backfill, then daily

v4 pools live in `data/base/uniswap_v4/pools.jsonl` (outside git, like every
inventory), one record per pool, folded from PoolManager's `Initialize` logs by
`scripts/data/v4_inventory.py`. Records carry no depth, so a v4 pool loads only
when the census targets it (a targeted pool loads below the floor). The loader
then applies the hook allowlist (`ops/v4-hooks.base.json`), and refuses native
currency, dynamic fees and protocol fees.

The one-time backfill is an agent step, as on 2026-10-28:

1. List the v4 pool IDs (the 32-byte `pools` entries) in the census's race
   files, `/opt/apex/var/census/race-*.json.gz`.
2. For each ID, read PoolManager's `Initialize` log through an explorer API:
   Basescan `getLogs`, address `0x498581fF718922c3f8e6A244956aF099B2652b2b`,
   topic0 `0xdd466e674ea557f56295e2d0218a125ea4b4f0f6f3307b95f85e6110838d6438`,
   topic1 the ID. Save the answers to one file.
   Or, for many IDs at once, resolve them through PositionManager's
   `poolKeys` (`--resolve IDS --rpc URL`, below). Each key is kept only when
   it hashes to its ID: `poolKeys` is keyed by the ID's first 25 bytes. Done
   this way on 2026-10-29: the census's 313 most valuable v4 pools of 10-21 to
   10-27, every one resolved. The 225 left out each carried under $0.19 a week.
3. Fold them in, and start the daily cursor at the block the backfill read to:

```
$ cd scripts/data
$ python3 v4_inventory.py --out ../../data/base/uniswap_v4/pools.jsonl --logs /path/to/initialize-logs.json
$ python3 v4_inventory.py --out ../../data/base/uniswap_v4/pools.jsonl --resolve /path/to/pool-ids.txt --rpc https://mainnet.base.org
$ echo <head block> > /opt/apex/var/census/v4-inventory.cursor
```

- The fold is idempotent by pool ID. It refuses a log that is not from
  PoolManager, not an `Initialize`, removed, or whose ID is not its key's hash,
  and it prints the counts.
- After that, `apex-targets.service` scans each day's new pools before the
  census: `--follow --rpc public`, 50 blocks a query over the census's public
  endpoints, never BlockPI's key. A failed scan does not stop the census.
- After the census it runs `scripts/data/target_pools.py`. That adds each
  targeted v2/v3 pool no inventory holds, read from the pool and proven by
  its own factory's `getPool`, with its hub-side depth.
  - On 2026-10-29 the first R37 targets named ten such pools, among them the
    four AXIS/USDC Uniswap v3 pools under the top v4 routes.
  - Two are 1%-fee pools: the universe's 3,000 ppm cap keeps them out.

### f. The pre-deploy shadow (the host)

Until the owner deploys `LeanExecutor2`, the evaluator simulates every fire
against its code, put at a placeholder address by a state override. The
override also sets the trader as an executor, a recipient, and the
executor's two Morpho approvals.

`ops/host/deploy.sh` packages the code as `dist/LeanExecutor2.runtime.hex`
(`forge inspect LeanExecutor2 deployedBytecode`). It refuses unless the code's
keccak equals the revision's `armed.executor2.code_hash`. It also packages the
v4 inventory with the others. On the host, after README §7's unpack:

```
# install -o apex -g apex -m 0644 /root/dist/LeanExecutor2.runtime.hex /opt/apex/var/armed/LeanExecutor2.runtime.hex
```

- `ops/v4-hooks.base.json` ships in the revision. Restart (README §7).
- The boot logs v4 pools loaded. The boot also checks two things:
  - the code file's hash is `code_hash`. On a mismatch, "LeanExecutor2 is not
    the configured code: nothing is armed";
  - every non-WETH/USDC token moves exactly. A token that fails is logged,
    with its pools never armed.
- A live boot refuses `runtime_code`.

### g. Gate A″: three days of v4 and USDC in shadow

Run at least three days. The gate passes when all of these hold:

- quote and simulation agree, to the wei, on ≥ 95% of v4 and USDC fires
  (`ops/host/remote.sh armed`); a USDC fire's gross is USDC units;
- every armed v4 pool's words matched the book (no "never armed" warnings);
- the first AXIS fires' simulated `gasUsed` sits within the arm's expected and
  limit (R37 Task 14 deferred this check to the first real fires);
- the fires that would have won, and their net per day, are counted against
  the actual winners.

**Passed, 2026-10-29, by the operator's decision, on about four hours of
armed-shadow, not three days.** Revision 5e28d54; arms from 04:33 UTC,
verdicts through 08:30 UTC.

- **Fires:** 3,261, all starting in USDC; 3,176 run through the AXIS v4 pool,
  simulated against LeanExecutor2's code before its deploy.
- **Quote and simulation agree to the unit on 97.9%** of v4 fires
  (2,789 / 2,848 at the last full count).
- **Gas:** v4 fires used 0.84× the arm's expected gas at the median, and none
  exceeded its limit. Our venues' USDC fires used 1.07×.
- **Health:** every armed v4 pool's words matched the book: 82 of 82 slots
  armable, no unarmable pool.
- **Would have completed**, at 900‰ of the expected surplus as the bid:
  - in the trigger's own flashblock: 526 won the race, 517 executed, 419 at a
    profit, net $107.73;
  - one flashblock later: 27 won, 24 executed, 18 at a profit, net $4.03.
  - A won fire whose simulation fails would be included and revert: 9 at
    900‰ (a hop short of its minimum).

The operator's decisions 2–4 followed the same day (§3h). Then §3a–c.

### h. The operator's decisions 2–4, 2026-10-29

1. **Decision 2, sender funding: "whatever necessary".** The float stays at
   0.002 ETH a sender, because that is what the shadow shows is necessary:
   - from 04:33 to 08:36 UTC, every arm that would have won at its own bid
     had a worst case of at most 0.00026 ETH;
   - so 0.002 ETH admits all of them, with about 8x to spare;
   - the arms that need more (bids up to 10 gwei on gas limits up to 3.1M,
     worst cases up to 0.0256 ETH) never won;
   - the 186 wins spent 0.00055 ETH in gas and tips between them.

   Refill with `FundArmedSenders.s.sol` when the report's `days_left` runs
   low. Raise the float only if a measured winner needs it: the script's
   `MOST` (0.01 ETH) and the 0.01 ETH loss budget would both have to rise.

2. **Decision 3, the live gate's threshold: none.** Gate A″ was the last
   gate. The owner's deploy (§3a–c) and the pilot under R36's caps come next.

3. **Decision 4, the daily loss budget: "losses are counted as losses".**
   - A win gives back none of the budget a loss spent. The day's loss is each
     losing landing's loss plus each abandoned worst case. `LiveState::admits`
     counts it with everything pending, and reaching the budget halts the
     lane until the next UTC day.
   - A USDC landing's net values the USDC paid, in wei, at the rate its arm
     was costed at (`Arm::start_per_eth`). A USDC win is therefore no loss.
     A landing with no rate on record counts all its gas as lost.
   - The report shows both: `loss_today_wei` (what the budget counts) and
     `net_today_wei`.

What the pilot's own bids would have made: the engine bids
`best_bid` against the route's measured winners (median 0.005 gwei a gas),
not the evaluator's 900‰ of the expected surplus. Over the same four hours,
at its own bid:
- 186 fires would have landed first (178 through v4), all profitable, net
  $82.83;
- 131 of those won by position, the rival landing in a later flashblock, and
  55 by bid;
- that is against 570 wins and $117.29 at 900‰, whose wins include 107 at a
  loss.
