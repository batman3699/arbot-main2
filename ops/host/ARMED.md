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

Switched on 2026-10-29 (UTC), the operator's decision, after gate A″ (§3g),
the owner's deploy (§3a–c) and six of six real-contract simulations agreeing.

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
reserve: run Fund again. `skipped.size` counts arms over `max_size_wei`
(`max_size_usdc` for a USDC arm). Since R39 the engine ranks no rung over its
cap, so this stays at 0: a count means the engine and the worker disagree.
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
- R41.2: winners bid by the gap they take, so each route's bids are banded
  by gap value (under $1, $1-3, $3-10, $10 up). A rung bids against its own
  band's winners, or the route's R36 list where the route has fewer than
  twenty there. The race census reads receipts from $1 only, so the census
  also reads 40 of each selected route's cheaper trigger-block winners a day
  into the day's cache ("read N cheap winners' receipts"). The first run
  backfills the seven days, about 20 minutes over the public endpoints;
  after that it is about 3 minutes a run.
- Why (2026-11-05): with $1 winners alone, the AXIS/USDC route's sample was
  the 10-29 spike's 3.2 gwei bids, so its cheap rungs ranked at about nothing
  and R39 stopped landing. Its cheap winners bid 1-80 Mwei a gas.

### Denim (since 2026-11-05)

Base's Denim hardfork activated at **2026-11-05 18:00:00 UTC** (timestamp
1793901600, from the `base/base` v1.5.1 client): 200 ms blocks, and no
flashblocks, `pendingLogs` or `pending` tag. R41
(`docs/superpowers/specs/2026-10-31-denim-readiness-design.md`) switched the
host on chain at the fork, with two systemd drop-ins; R41.1 made
`ops/canary.base.yaml` and `ops/shadow.base.yaml` the Denim configs, removed
the drop-ins and retired the switch (README §12). The switch's record is
`/opt/apex/var/apex/denim-switch.jsonl`.

**The regime.** Seven fields set it, and a test holds both files to them:
- `armed.block_time_ms: 200` and `armed.max_head_lag_ms: 1000`;
- `feed.pending_logs: false`, and no `flashblocks:` section;
- `policy.l1_every_blocks: 300` and `policy.max_evidence_age_blocks: 18000`
  (a minute and an hour);
- `policy.head_reads_every_ms: 2000`: the view, the TWAPs, the fees and the
  stops are read at most every 2 s, the 2 s regime's rate in wall time.

Armed-live sends only while the last twenty heads' own `timestampMs` keep
200 ms and the newest is at most 1 s old at the send; a head without
`timestampMs` stops sends. No request names `pending`. W is 10 s, 50 blocks,
inside Base's 60 s bound limit. A config that is half one regime and half the
other is refused at load, naming the field.

A booting process sends nothing until the reopened ledger's pending
transactions are settled, 20 stamped heads show 200 ms, the newest head is at
most 1 s old, the stops are fresh, and there is no kill file or halt; each is
checked again before every send.

**Checking it** (gate M's checks, at any time):
- the last report's `armed.live`: `chain_cadence_ms` 200, `head_lag_ms` under
  1000, `head_skips` counted, `heads_without_ms` 0; `stops_reads` and
  `head_reads.rounds` each rising by about 30 a minute, never more;
- the book is not Rebuilding, logs arrive every minute, no reconnect storm (a
  check by eye; the predicates are what stop a stale book from trading);
- the arms carry a bound of head + 50 and `start_at`, and are accepted;
- `rpc` (HTTP bytes by method) per hour within 1.5x of the 2 s regime's
  (R41's boot, 11-04: about 190 MB/h, 167 MB of it `eth_call`); `feed.bytes_heads`
  about 10x it and no `bytes_pending_logs`, the feed about 0.9x in all;
- `denim-check` against BlockPI for 5 minutes; the verdict is the last line:

```
# systemd-run --uid=apex --wait --pipe -p WorkingDirectory=/opt/apex -p EnvironmentFile=/etc/apex/shadow.env /opt/apex/bin/apex denim-check --config ops/canary.base.yaml --for-s 300 | tail -1
```

**Responses:**
- **refusals naming the bound:** set `armed.window_s: 6` (30 blocks) in
  `ops/canary.base.yaml` and restart;
- **BlockPI frozen or storming:** `touch /opt/apex/var/armed/armed.disable`,
  test the key over HTTP for the error code (402 or 503 is prepaid RU run
  out); then wait, or a second websocket (R41 decision 4);
- **lag p99 over 1000 ms while BlockPI is otherwise healthy:** the guard is
  idling the lane on delivery. Raise `armed.max_head_lag_ms` (up to 5000) only
  if the lag is steady rather than growing;
- **RU rising:** stop `apex-canary` and read the per-method counters.

**There is no going back to 2 s.** The kill file stops sends at once and the
process keeps measuring; stopping `apex-canary` stops the RU.

**Not under Denim:** `apex send-probe` (it reads the nonce at `pending`).
`apex armed-moves-seed` turns hours into blocks at the configured block time,
so a seed must not reach back across the fork; `moves.json` persists across
restarts, so a restart needs no seed.

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
$ forge build script/DeployLeanExecutor2.s.sol
$ forge script script/DeployLeanExecutor2.s.sol --sig "run(address,address)" 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3 0xCB436Ba3acb945b3fc8EE6345857262584356595 --rpc-url https://mainnet.base.org --sender 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3
$ (the same, with --broadcast --interactive)
```

- It prints the address and the runtime code hash, which become
  `armed.executor2.address` and `.code_hash`.
- The script refuses another chain. It checks the roles and Morpho's USDC
  approval before anything is broadcast.
- Build the script's own files first ("Compiling 18 files"). Under via_ir the
  bytecode depends on what is compiled together:
  - the 18 files give code hash `0xb67e…9c7e`, which the shadow simulated and
    `forge test` ran on;
  - `forge script` straight after `forge clean` compiles 40 files, and a full
    build 83; both give `0x1de1…fd38`.
- Compare the dry run's printed hash with `code_hash` before broadcasting.
  Each broadcast deploys another copy, at the owner's next nonce.
- Done on 2026-10-29 (UTC): `0x443A997cF27Dac4BB32e007d4168492abC77ad6e`,
  owner nonce 89, block 53247811, code hash `0xb67e…9c7e`, 1,947,588 gas.
  Two other copies are unused and hold nothing; on each, the trader is still an
  executor:
  - nonce 88, `0x29b92ef48A001a76b2E649Fe309356ABD8bf66f6`, the 40-file build
    (`0x1de1…`);
  - nonce 90, `0x3F594BafD57aBc31F3db084713867bEb86d751Ed`, `0xb67e…`.

### b. Authorize the 16 senders on it (the owner)

Set `executor` in `ops/armed-senders.json` to the new address. Then dry-run
and broadcast `AuthorizeArmedSenders.s.sol` exactly as in §1c, and check that
every sender reads true. The senders are the same 16, so no new keys are
needed. They stay at 0.002 ETH each (decision 2, §3h).

Done on 2026-10-29 (UTC): owner nonces 91–106, all in block 53248030,
739,256 gas, and all 16 senders read true on `0x443A…`. They are still
allowed on the R29 executor `0xd25B…f2c5` too (§3d).

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

**Correction, 2026-10-29 (the first live hour):** most of those wins by
position were not wins.
- A trigger that is its flashblock's last transaction cannot be followed in
  that flashblock. The contest moves to the next one and is decided there by
  priority fee.
  - Live arm 3114's trigger closed flashblock 1. Our 0.005 gwei expired, and
    the rival's 0.053 gwei landed in flashblock 2.
  - The evaluator had called it a win by position.
- Re-judged from 04:33 to 18:56 UTC:
  - 245 own-bid wins ($84.28) become 113 ($6.31);
  - 135 of the 245 ($78.00) were that case.
- The evaluator also read our own live landings as a rival's.
- armed-eval judges both correctly from R37's fix on, and judges each arm's
  own bid (`own`). Figures above this note predate the fix.
- The bid model's measured contest is the subject of the R38 draft,
  `docs/superpowers/specs/2026-10-29-armed-bid-from-own-fires-design.md`.

## 4. R38: the bid against our own fires

The design is `docs/superpowers/specs/2026-10-29-armed-bid-from-own-fires-design.md`
(approved 2026-10-29).

- **The census measures it.** Each day `apex-targets` also writes a `contest`
  section into `targets.json`.
  - Its rows are, for each fire armed-eval judged, the share of the arm's
    surplus a bid had to exceed to win (`arb_census/contest.py`).
  - The fit is three complete UTC days from 2026-10-29 on, and the next day
    is held out.
  - `fit_ok` holds only when calibration and the paired net against
    `best_bid` pass on the held-out day, and `reasons` says why not.
  - It never changes the targets' own `ok`.
- **Every arm carries both bids.**
  - `bid_model` names the rule that sent it;
  - `alt_bid_wei_per_gas` and `alt_p_win` are the other rule's;
  - `model_version` names the sample.
- **armed-eval judges three bids on every fire:** `own`, `alt`, and `base`
  (1 wei). The report's summary has them by day.
- **The switch is `armed.bid_model: best_share`** in `ops/canary.base.yaml`,
  then a deploy and a restart. It is the operator's call, after the gates
  and a full day of `alt`.
  - Without a fit sample (absent, unfit, or under 300 effective rows), the
    arms send `best_bid` and record `best_bid (fallback: <reason>)`.
  - The boot logs the sample: `the arms bid with the census's contest
    sample`, or `no contest sample`.
- **History.** The fit reads `var/apex`: verdicts, arms and flashblock
  ledgers.
  - README §7's archive of `var/apex` moves them.
  - Until four days have passed again, the sample is unfit and the arms send
    `best_bid`.


## 5. R40.1: the sized lane (`LeanExecutor3`)

The plan is `docs/superpowers/plans/2026-11-02-r40-1-any-size-armed-lane.md`.
`LeanExecutor3` sizes each trade itself at execution, and prices its floor at
the transaction's own gas price. Its engine runs beside R39's on the same
feed. One lane sends at a time; the other only records (R43, §6d: both,
when this one sends its spike tiers alone).

### a. Shadow, before the deploy (the host)

Every shipped config carries `armed.sized` in `mode: shadow`, at a
placeholder address with `runtime_code`. `ops/host/deploy.sh` packages
`dist/LeanExecutor3.runtime.hex`, and refuses unless its keccak is the
revision's `armed.sized.executor3.code_hash`. On the host, after README §7's
unpack:

```
# install -o apex -g apex -m 0644 /root/dist/LeanExecutor3.runtime.hex /opt/apex/var/armed/LeanExecutor3.runtime.hex
```

- The boot logs "the sized lane arms beside R39's". On a hash mismatch it
  logs "LeanExecutor3 is not the configured code: the sized lane arms
  nothing", and R39 runs on alone.
- Its arms go to `var/apex/armed-sized-*.jsonl`, never to the worker.
- `apex armed-eval` judges them into `var/apex/armed-sized-verdicts-*.jsonl`,
  past its own cursor, `armed-eval-sized.cursor`. Each verdict's `sized`
  field says what the executor would have done at the fire: `traded`,
  `capped`, `truncated` or `early_exit`, with what the cap and the step
  limit left.

### b. Gate C: one post-Denim UTC day

Pass means no correctness failure: book and chain storage agree; every
simulated fire's `sim.agrees`; the reservations and budget hold; overlapping
arms behave. Early exits, capped and truncated value, and the comparison
with R39 are reported, not gated.

`ops/host/remote.sh armed-sized` prints both lanes' `own` by day (the arms'
own bids, pessimistic and optimistic: `r39_own` and `sized_own`), and the
sized lane's `sized_days`; `apex armed-eval --summary` has it all, R39's at
the top level and the sized lane's under `sized`. For each day, `sized_days`
give the fires by outcome, `beyond_cap_usd` and `beyond_steps_usd` (what the
cap and `maxSteps` left), `ladder_end` (fires the book's ladder ended before
the walk: the mirror short of the chain), and `simulated` against
`sim_agreed`, which must be equal.

### c. Deploy, authorize, flip (the owner, then the host)

1. Forge clean, `forge build script/DeployLeanExecutor3.s.sol`, a dry run, then
   the owner broadcasts. The printed code hash becomes
   `armed.sized.executor3.code_hash`, the address its `address`, and
   `runtime_code` goes.
2. Authorize the senders in one transaction:
   `forge script script/AuthorizeArmedSenders.s.sol --sig "runBatch()"`, with
   `ops/armed-senders.json`'s `executor` set to the new address.
3. Flip: `armed.sized.mode: live` and `armed.mode: shadow`, then restart.
   The worker now sends to `LeanExecutor3`, with `armed.sized.max_senders`
   keys.

Rollback: the same two fields back, and a restart. `LeanExecutor2` stays
deployed and authorized.

## 6. R43: spike tiers on the sized lane

Over 2026-11-03..09, 41 trades of $100 or more held 56% of the value on the
routes this lane trades, and their winners paid a median 15 gwei. No arm
could bid that: a sender held 0.002 ETH, the cap was 20 gwei, and at 20 gwei
the sized lane's gas limit alone is twice the day's budget. A spike tier is
a standing arm for such a move, with its own threshold, bid, senders, size
cap and budget rule. Nothing below takes effect without the yaml.

### a. The keys (`armed.sized`)

```
reserve: no_trade                # what a pending spike arm holds of the budget
daily_loss_budget_wei: ...       # the spike tiers' own; armed's when unset
live_ledger: var/armed/armed-live-sized.jsonl   # only when both lanes send
tiers:
  - least_net_wei: ...           # the move must open this, before any tip
    bid_share_bps: 2500          # tipped: a floor under the model's bid
    max_priority_wei_per_gas: ...
    senders: 6
    max_size_wei: ...            # armed's when unset
    max_size_usdc: ...
```

- A tier's threshold is the smallest move whose net reaches `least_net_wei`.
  Its bid is the larger of the model's and `bid_share_bps` of that net over
  the trade's gas, under the tier's own cap.
- Senders: the ordinary tiers arm on the first slots, each spike tier on its
  own after them, in the tiers' order. With `max_senders: 16` and tiers of 6
  and 2: slots 0-7, 8-13, 14-15. A tier's senders go to the routes the census
  values most, one arm for each trigger pool and direction.
- `reserve` is the spike tiers' alone (R43.1). `worst_case` holds the gas
  limit at the arm's fee, as before. `no_trade` holds 140,000 gas at it: what
  an inclusion that does not trade costs. The ordinary tiers always hold
  their worst case, and their pending arms stay inside `armed`'s own budget
  whatever the lane's is. A trade nets its floor after its own gas. A revert is not covered:
  it can cost up to the gas it used at the arm's fee, and the day's loss cap
  stops the lane only afterwards. A sender can never lose more than its
  balance.
- The ceilings are `armed`'s own: 1,000 gwei, 100 WETH, 100,000 USDC, 0.1 ETH
  a day, at most half the net tipped, at most three tiers.

### b. Funding a tier's senders (the owner)

The protocol holds a sender to its gas limit at its fee, whatever it then
uses, and the worker to `armed.sender_reserve_wei` beside it:

```
balance >= 1,000,000 x (2 x base fee + max_priority_wei_per_gas) + l1_fee_bound_wei + sender_reserve_wei
```

A 20 gwei tier needs about 0.0205 ETH a sender; a 157 gwei tier about 0.158
ETH. A sender short of that is skipped as `balance`, every time. The slots
are positions in the key file; take the addresses from
`ops/armed-senders.json`, in its order.

### c. Shadow first

In `mode: shadow` the tiers arm and record, and nothing is sent. Shadow needs
no keys, so the shipped config can add the spike senders to the ordinary
sixteen (`max_senders: 24`) and leave gate C's lane as it was. Read it with
`ops/host/remote.sh armed-sized`; a spike arm's line carries `sized.tier` 3
or more and `bid_model: tier_share`. The report's `armed.sized_engine` has
the sized engine's arms and its last pass's time, beside R39's `engine`. Each tier's senders should hold arms on
target routes; `beyond_cap_usd` says what the tier's size cap left.

### d. Both lanes live

`armed.mode: live` with `armed.sized.mode: live` boots only when the sized
lane's tiers hold every one of its senders and it names `live_ledger`. Then:

- the key file's first `armed.max_arms` keys are R39's worker, to
  `LeanExecutor2`; the next `armed.sized.max_senders` are the sized lane's own
  worker, to `LeanExecutor3`, with its own ledger and day's budget;
- each worker checks its senders against its own executor at boot, so the
  owner authorizes the spike senders on `LeanExecutor3` first (§5c step 2,
  with only those addresses);
- the key file must hold exactly `max_arms + max_senders` keys;
- the report's `armed.live_sized` is the second worker's section; the kill
  file stops both.

Rollback: `armed.sized.mode: shadow` and a restart. R39's lane is untouched.

### e. The feed's fallbacks (`rpc.ws`)

`rpc.ws` is one endpoint or a list. A session that ends having delivered
nothing moves the feed to the next endpoint; one that was delivering is
reconnected to, and after it the first endpoint is tried first. `rpc.http`
takes a list too; its reads stay on whichever endpoint last answered, so
after an outage they return to the first at the next restart.

The shipped configs name publicnode second in both. Probed from the host on
2026-11-13 (`apex denim-check` on a copy of the config, ten minutes): every
subscription accepted on one session, 3,000 of 3,000 heads, all with
`timestampMs`; head lag p50 73 ms, p99 4,216 ms, so the guard held for 97.9%
of heads against the check's 99%. It keeps the run up through a provider
outage with the lane idle about 2% of the time; it is not a replacement. Of
eight other public endpoints none served the feed: no websocket, refused
subscriptions, throttling, or heads without `timestampMs`.

While the keyed provider is out, the report's `rpc` and the journal's
"rotating" and "websocket session ended" lines name the endpoint in use. The
canary's own sends go to `rpc.http[0]` and the sequencer, never to a
fallback.
