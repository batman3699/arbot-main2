# Denim readiness (Task 8.5 R41)

2026-10-31 (NZ). The operator asked for it after R40.1's revision: "yes,
write the R41 Denim design". The same day they supplied an independent
review (`~/Documents/R41_Denim_readiness_profit_capture_review_2026-10-31.md`)
"just for your consideration". This revision answers it point by point
(§Review response).

**Approved 2026-10-31** by the operator: "yes, approve R41 as revised, write
the plan". The decisions are taken as recommended: 1 the scope, 2 the
automatic switch, 3 live through the fork, 4 BlockPI as the single
websocket, 5 deploy on 11-03 without archiving. For 6, no BlockPI Sepolia
key was supplied, so gate S rehearses on public endpoints. The plan is
`docs/superpowers/plans/2026-10-31-r41-denim-readiness.md`.

Every call I made is marked **(my call)**. The operator's decisions are
listed last. Times are UTC unless marked NZDT.

## Why

Base's Denim hardfork activates on mainnet at **2026-11-05 18:00:00 UTC**
(timestamp 1793901600; 07:00 NZDT Friday 6 November). The source is the
stable `base/base` v1.5.1 release (2026-10-22): "Operators must upgrade to
this release by November 5 at 18:00:00 UTC for the Denim hardfork".
The client agrees: at tag v1.5.1, `crates/common/chains/src/chain.rs`
asserts Denim inactive on mainnet at 1_793_901_599 and active at
1_793_901_600 (Sepolia: 1_792_605_600). docs.base.org still says only
"November 2026". At the last check (2026-10-30 20:09) that was 142 hours
away.

From that block on:
- blocks come every 200 ms, not every 2 s;
- flashblocks, `pendingLogs` and preconfirmation state end;
- `block.timestamp` stays in whole seconds, and headers gain `timestampMs`.

R36 built a config switch for this day (`ops/host/ARMED.md`, "The Denim
day"). It has never run against a 200 ms chain, it needs someone to flip
it at 07:00 NZDT, and it doesn't cover everything. This is what today's
code and config do at 18:00:

| Part | What happens | Effect |
|---|---|---|
| Armed sends | The cadence guard sees 200 ms heads against 2,000 configured and stops every send within a few seconds. Pending arms (bound: head + 5) expire within a second. | The lane is idle until someone edits two configs and restarts. Idle, not a loss. |
| The capture feed | It subscribes to `pendingLogs`. If BlockPI refuses that after the fork, the whole session ends and reconnects every 10 s or less (`ws.rs`: by design, a refused subscription is "not run"), and the book stays Rebuilding. PublicNode on Sepolia accepts it and stays silent. BlockPI's answer is unknown. | Blind, if refused. |
| Per-head reads | Every head runs the reader's view (six calls), Slipstream's TWAPs, Aerodrome v2's fees and the armed stops multicall. At 5 heads a second that is 10× today's rate. These reads are most of the measured 3.8 GB/day of HTTP. | This happens after a manual flip too. BlockPI usage would be roughly 4–5× today's, and prepaid RU has already run out three times. Blind within days. |
| The `pending` tag | The reader reads the trader's nonce at `pending` on every head. Tier 2 simulates at `pending`. Base's own Sepolia node answers it as `latest`. BlockPI's mainnet node is unknown, and Base's docs only say it has "no direct equivalent". | If BlockPI rejects it, the view never refreshes, so the armed lane's lender holdings go stale. |
| The evaluator | With no flashblock ledger it falls back to whole blocks, which is roughly right. It cannot see a trigger that is its block's last transaction, the case that made about 93% of the "won by position" verdicts artefacts. It also settles after 30 blocks, which becomes 6 s instead of a minute. | R38's and R40.1's refits on post-Denim days would be biased. |
| The census | Post-fork days are already ranged by block timestamp (`targets.py`). The trigger search window is 150 blocks ("five minutes"), which becomes 30 s. A day is 432,000 blocks, so 10× the `eth_getLogs` calls on public endpoints. | If targets stop being written for 2 days, the canary refuses to boot at its next restart (`targets_max_age_s`). |
| The reactive lane (parked) | Tier 2's block clock is hard-coded at 2 s (`sim.rs` `BLOCK_TIME_S`), and the capacity sampler subscribes to `newFlashblocks`. | Wrong simulations, errors, and RU spent on noise. No sends: `canary.stop` stays in place. |

## Goal

The armed lane trades on Denim **within 5 minutes of the fork, with no one
awake**, and stays live after it:
1. BlockPI usage per day on Denim is no higher than before the fork.
2. The evaluator and the census measure in blocks correctly, so post-Denim
   days can refit R38's bid and set R40.1's numbers.
3. Nothing in the system can lose more after the fork than it can today.

**Done means** (gates M and D below):
- the switch fires on the ten-block lattice proof;
- chain cadence and head lag pass the guard;
- arms are accepted with bounds 50 blocks out, and carry `start_at`;
- per-head reads run at most every 2 s;
- no `pending` appears in any request;
- the evaluator judges post-fork arms in block mode, including blocks the trigger closes;
- targets are written for 11-06 and 11-07.

## Non-goals

- **The reactive lane.** It stays stopped (`canary.stop`). R41 only stops
  it erroring or spending RU after the fork. Re-enabling it is a separate
  decision.
- **R25's race ledger and flashblock analyses** (`race_analysis.py`, the
  contest tools that need the flashblock clock). They end with the stream.
- **R23's market census tools** (`collect.py`, `report.py`, which carry
  blocks-per-day constants). They are analysis tools, not on the daily path.
  They are left as they are, and their constants stay valid for pre-fork ranges.
- **Switching regime inside a running process.** The switch is a restart
  between two configs (§1–2). The cadence guard covers the seconds between.
- **A second websocket provider.** See decision 4.
- **R40.1.** It waits for R41. Its numbers come from post-Denim days.

## Anti-goals (a failure even if it works)

- **A fork-day change that can send when the config and the chain disagree.**
  Today a mismatch idles the lane. That property stays.
- **A pre-fork deploy that changes behaviour before the fork.** Everything
  R41 adds is off in today's configs. The pre-fork run must look identical
  in the status lines.
- **A switch that fires on the wrong chain state.** If Base postpones the
  fork, nothing changes.
- **Measurements that silently mix units.** Offsets in flashblocks and in
  200 ms blocks are both about 200 ms units, and are labelled as such. Block
  offsets from 2 s blocks are never mixed with 200 ms ones as if they were
  the same unit.
- **Any change to the bid, the tier model or sender funding at the fork.**
  `best_bid` is held, the 16 senders and the 0.01 ETH budget stay, and
  R38's 11-02 gates switch nothing before the post-Denim refit (11-10 at
  the earliest).
- **Reading R41's gates as evidence of profit.** They prove the lane runs
  and measures correctly on Denim, nothing more. R40.1 keeps its own gates.

## Constraints

- Six days. The build and its deploy finish by **11-03**, leaving 11-04 for
  rehearsal (timeline below).
- The standing rules hold:
  - no secrets in argv or logs;
  - named-path staging, no `cargo fmt`, `-j 3`;
  - five CI gates before any push;
  - the operator approves each merge, push and deploy;
  - no getLogs bursts on the BlockPI key while the canary is live;
  - `canary.stop` stays in place.
- No new non-secret environment variables. New settings are config fields.
- The BlockPI key is mainnet-only ("apikey is on another chain" on Base
  Sepolia). Sepolia rehearsal uses public endpoints unless decision 6
  changes that.

## Facts verified, and what still needs checking

**Verified on 2026-10-30, about 20:05–20:15 UTC:**
- **Mainnet head 53,300,795** (20:08:57):
  - no `timestampMs`;
  - gas limit 400M a block;
  - base fee 5,000,000 wei (the floor);
  - `extraData` gives denominator 100 and elasticity 5.
  - If 2 s blocks hold, the first Denim block is about **53,556,126**: the
    first with timestamp ≥ 1793901600.
- **Base Sepolia, Denim since 2026-10-21 18:00 (1792605600):**
  - 300 blocks in 60.0 s, by `timestampMs`;
  - `timestamp` in whole seconds, five blocks to a second;
  - gas limit 120M a block, base fee 5,000,000;
  - `extraData` gives denominator 1000 and elasticity 15, so the base fee
    moves at most 1.4% a block.
- **Base's own Sepolia node (`sepolia.base.org`)** answers `pending` for
  `eth_getTransactionCount`, `eth_call`, `eth_getBlockByNumber` and
  `eth_simulateV1`, apparently as `latest`.
- **PublicNode's Sepolia websocket:**
  - It accepts `pendingLogs` and `newFlashblocks` and then sends nothing on them.
  - `newHeads`: 146 in 30 s, none skipped.
    - Arrival intervals: p10 109 ms, median 204, p90 272, max 428. Three of
      145 arrived under 50 ms after the last.
    - Each head message is 1,845 bytes, so 0.8 GB/day at 5 a second, against
      0.08 today.
  - `logs`: canonical, about 60 a second.
- **Re-measured 2026-10-31 ~02:45 UTC** (PublicNode Sepolia `newHeads`, 60 s,
  from NZ on an NTP clock within 0.5 ms):
  - 294 heads, every one with `timestampMs`; none skipped or repeated.
  - Every step between consecutive heads is exactly 200 ms (293 of 293),
    every `timestampMs` is a multiple of 200, and `floor(timestampMs / 1000)`
    always equals `timestamp`.
  - **Delivery lag** (arrival − `timestampMs`): min 113 ms, p50 166, p90 238,
    p99 347, max 487. That is from NZ; the host is in us-east-1.
- **The sequencer seals a Denim block 150 ms into its slot**
  (`DEFAULT_SEAL_OFFSET`, `crates/consensus/protocol/src/timing.rs`, v1.5.1).
- **Base's docs** (`/upgrades/denim/migrate-from-flashblocks`, `/200ms-blocks`):
  - `pendingLogs` → `logs`; `newFlashblocks` → `newHeads`.
  - The `"pending"` tag "has no direct equivalent".
  - They don't say whether the old subscriptions error.
  - Logs gain `blockTimestampMs`.
- **Code** (paths under `crates/apex-runtime/src` unless named):
  - `apex-chain/src/rpc/ws.rs` ~470: a refused subscription ends the
    session, and the feed reconnects.
  - `armed/live.rs` 473: the stops multicall runs on every head.
    - 784: stops are fresh for `3 × block_time_ms`.
    - 789: the cadence guard is the median of 20 arrival intervals, within
      half either way.
  - `shadow/mod.rs` 1305–1360: `head_loop` reads the view, TWAPs and
    Aerodrome v2 fees on every head.
  - `live/reader.rs` 133: the nonce at `pending`.
  - `live/sim.rs` 299: `BLOCK_TIME_S = 2`; 362: `pending`.
  - `armed/run.rs` 52: `SETTLE_BLOCKS = 30` ("a minute").
    - 527: whole-block placement when the ledger has no flashblocks.
  - `shadow/mod.rs` `capacity_loop`: without a model it samples
    `newFlashblocks` again every minute.
  - `main.rs` 550: the moves seed turns hours into blocks at the configured
    block time.
  - Policy fields in blocks: `max_evidence_age_blocks` 1800 (an hour) and
    `l1_every_blocks` 30 (a minute).
  - `scripts/data/arb_census/race.py` 46: `BACK_BLOCKS = 150`.
  - `targets.py` ranges a day without a flashblock ledger by block timestamp.
- **The arm's calldata deadline** is in seconds (`engine.rs` 563), so it is
  unaffected. Only the validity bound is in blocks.
- **What a stale book can do on chain** (`armed/predicate.rs`): every arm's
  predicates hold the trigger pool past its threshold, **and** the other
  pool's price inside its band with its liquidity above a floor. The builder
  checks them against the state right before the transaction would run. An
  arm priced on a book the chain has left matches nothing, so it is not
  included and pays nothing. A floor in the calldata covers what is left.
- **The feed's continuity rules** (`live/feed.rs`, table at the top):
  - a reconnect, or notifications dropped on a full channel, applies what is
    held, marks the whole book Rebuilding and re-reads it, and nothing is
    priced until then;
  - a removed log (a reorg) reloads its pool;
  - a log not newer than its pool's last is refused, so order is kept per
    pool.
- **The evaluator's order** (`armed/eval.rs` `judge`): effects run in
  `(block, tx_index)` order. The fire is the first transaction after which
  every predicate holds. The rival is the first transaction **after the
  fire** that makes one false. Positions are recorded as block, transaction
  index and flashblock (`run.rs` `Position`).
- **What the per-head reads are** (`shadow/mod.rs` `head_loop`, `armed/live.rs`
  `on_head`): the signer's nonce (a tripwire), lender and PoolManager
  holdings, Slipstream's 10-minute TWAPs, Aerodrome v2's fees, the L1
  oracle (already every 30 blocks) and the owner's stops. None of them is
  pool state. Prices, liquidity and the tick in Slipstream's dynamic fee
  come from the log feed and the book.
- **The census reads public endpoints** (drpc, 1rpc: `race.py` `ENDPOINTS`;
  `apex-targets.service` `--rpc public`), never BlockPI. Only the evaluator
  (`apex-armed-eval`) shares the BlockPI key with the canary.
- **The HTTP usage counters already count bytes in and out by method**
  (`apex-chain` `failover.rs` `MethodUsage`, R28.1, in the report's `rpc`).
  The websocket counts no bytes. BlockPI bills by the byte. (Corrected while
  planning: the first revision said the HTTP counters count calls only.)
- **The host has no alerting.**

**Can only be checked at or after the fork:**
- whether BlockPI refuses `pendingLogs` or the `pending` tag;
- whether BlockPI's heads carry `timestampMs`, and their lag from the host;
- whether the sequencer takes a 50-block bound (its rule is 60 s, so it
  should);
- mainnet's Denim fee parameters;
- whether BlockPI's node is upgraded on time.

Each has a safe failure, an idle lane, and a named response (Gate M).

## Design

### 1. Two regimes, two configs, one check

The repo gains `ops/canary.denim.yaml` and `ops/shadow.denim.yaml`. Each is
its base file with exactly these changes:

| Field | Base (2 s) | Denim (200 ms) |
|---|---|---|
| `armed.block_time_ms` | 2000 | 200 |
| `feed.pending_logs` | true (absent) | false |
| `flashblocks:` section | present | absent |
| `policy.l1_every_blocks` | 30 | 300 (a minute, as now) |
| `policy.max_evidence_age_blocks` | 1800 | 18000 (an hour, as now) |
| `policy.head_reads_every_ms` (new, §3) | absent (every head) | 2000 |
| `armed.max_head_lag_ms` (new, §5) | absent (0: off) | 1000 |

**Load-time check (my call).** `block_time_ms` must be 2000 or 200.
- 200 requires `pending_logs: false`, no `flashblocks` section, and
  `max_head_lag_ms` between 1 and 5,000.
- 2000 requires `pending_logs: true` and `max_head_lag_ms` 0: pre-fork heads
  carry no `timestampMs` to measure lag against.
- Anything else is refused at load, naming the field. This rules out a
  half-flipped config.
- `ShadowConfig::denim()` is true for a 200 ms config. Every regime-dependent
  path below asks that one function.

`policy.head_reads_every_ms` must be 0 or 1,000–10,000 **(my call)**.

**Drift test.** A test parses each pair as plain YAML and asserts the two
files are equal except for the fields in the table, at the table's values.
Then a later edit to `canary.base.yaml` that forgets `canary.denim.yaml`
fails CI. The same test loads all four files through `ShadowConfig`'s real
loader, so each must also pass the load-time check.

### 2. The switch at the fork: automatic, chain-gated

A one-shot unit on the host, `apex-denim-switch.service`, started by
`apex-denim-switch.timer` at **2026-11-05 17:59:00 UTC**, runs
`ops/host/denim-switch.sh`:

1. **Detect.** Every 2 s, read `eth_chainId` and the newest ten blocks, in
   one batch, from `https://mainnet.base.org` (public, no key). Denim is
   live when all of these hold:
   - the chain ID is 8453;
   - the numbers are consecutive;
   - every block carries `timestampMs`, a multiple of 200, with
     `floor(timestampMs / 1000)` equal to its `timestamp`;
   - each step is exactly 200 ms (measured on Sepolia: 293 of 293);
   - the oldest of the ten has a timestamp of at least 1793901600, the
     value in the v1.5.1 client source.

   Ten 2 s blocks can pass none of the lattice checks. Requiring all ten
   to be post-fork means no pre-fork block is in the sample.

   **A second source (my call).** If `mainnet.base.org` doesn't answer, the
   same checks run against `https://base-rpc.publicnode.com`. If both answer
   and disagree, that round doesn't switch. One source that shows the
   lattice is enough: a false switch onto a 2 s chain idles the lane anyway,
   because the Denim guard (§5) needs `timestampMs`. Requiring two sources
   would only add a way for the switch to fail.
2. **Switch.** Write systemd drop-ins (`/etc/systemd/system/apex-canary.service.d/denim.conf`
   and the same for `apex-armed-eval.service`). Each one points `ExecStart`
   at the matching `*.denim.yaml`. Then `daemon-reload` and
   `restart apex-canary`. The armed-eval timer picks its drop-in up on its
   next run.

   Drop-ins, not file copies, because a later deploy unpacks the tree and
   would quietly put the 2 s config back **(my call)**.
3. **Record.** Append one JSON line to `/opt/apex/var/apex/denim-switch.jsonl`
   with the source, the ten blocks' numbers and `timestampMs`, and the
   restart time. A round that saw something unexpected (a wrong chain ID, a
   gap, an off-lattice step) writes a line too.
4. **Give up.** If Denim isn't detected by 20:00, write a `gave_up` line and
   change nothing. If Base postpones the fork, the operator re-arms the
   timer for the new date.
5. **Idempotent.** A run that finds the drop-ins already present does nothing.

The script takes `--rpc`, `--fork-at` and `--dry-run`. A dry run prints what
it would write and writes nothing. That is how gate S rehearses it against
Sepolia (`--fork-at 1792605600`), where Denim is already live.

**After the fork (R41.1, about 11-06):**
- a commit makes the base configs the Denim ones;
- the drop-ins are removed;
- `ARMED.md` drops the 2 s regime.

Until then, the drop-ins are the record of what the host runs.

**In the gap.** From 18:00 to the restart, the old process's guard stops
sends within about ten 200 ms heads, and pending arms expire on their
5-block bounds within a second. The new process boots, and sends nothing
until every one of these holds:
1. the reopened ledger's pending transactions are settled (their bounds
   have passed);
2. 20 heads (4 s) have filled the guard, and their `timestampMs` steps
   show 200 ms (§5);
3. the newest head is no more than `max_head_lag_ms` old (§5);
4. the owner's stops have been read and are fresh;
5. no kill file, and no halt in the ledger.

Each is checked again before every send. If any fails, nothing is sent.

### 3. Per-head reads on a clock

A new field, `policy.head_reads_every_ms` (default 0: every head, exactly as
today). `head_loop` starts a round of reads (view, TWAPs, fees) on a head
only if at least that long has passed since the last round started. The
heads in between are skipped. The armed live worker's `on_head` does the
same for the stops multicall.

`on_head` still sees **every** head for the cadence guard and for settling
expired senders. Settlement is driven by bounds expiring, so its rate in time
doesn't change. The view's age limit (`max_view_age_ms`, 6,000) still holds
with a round every 2 s.

**Only slow values go on the clock.** These are the signer's nonce, the
lender and PoolManager holdings, the 10-minute TWAPs, Aerodrome v2's fees
and the owner's stops (§Facts). A round every 2 s reads each of them as
often, in wall time, as today's 2 s heads do.
- Nothing whose value matters inside a block is on the clock. Prices,
  liquidity and Slipstream's tick term come from the log feed into the book,
  every block.
- The predicates check those on chain at the moment of execution.
- A 2 s-old TWAP moves Slipstream's fee by about 0.05 bps on a 100-tick
  move over 10 minutes. That is unchanged from today.

**Usage in bytes (my call).** The HTTP transport's per-method counters
already carry request and response bytes (R28.1). The capture feed gains
websocket bytes by subscription.
Both go in the report, so gate M compares bytes by method, which is what
BlockPI bills, not call counts.

**Stops freshness becomes `3 × max(block_time_ms, head_reads_every_ms)`.**
Under Denim that is 6 s, the same as today. Without this change, stops read
every 2 s would never be fresh under a 600 ms limit, and the lane would never
send. This exact pairing gets a test: block time 200, reads every 2,000, and
a send goes out.

**Usage, Denim against today (estimated from the measured sizes):**
- HTTP per-head reads: unchanged at one round per 2 s.
- Websocket:
  - heads rise from 0.08 to 0.8 GB/day;
  - dropping `pendingLogs` removes the preconfirmed copy of every log,
    about half of today's ~1.9 GB/day of logs.
  - Net, the feed is about 0.2 GB/day lighter.
- The evaluator's `getLogs` covers the same logs over 10× the blocks.
  - The bytes are about the same.
  - The call count rises at the 500-block chunk, from about 1 to about 4
    per filter per run. A run every 5 minutes spans about 6–7 minutes of
    blocks, 200 today and 2,000 on Denim. That's too few calls to matter, so
    the chunk stays.

### 4. No `pending` tag in the Denim regime

When `denim()`:
- the reader reads the trader's nonce at `latest`;
- Tier 2 simulates at `latest`.

The block context gets its clock from the config, not from
`BLOCK_TIME_S`:
- The block being built is the head plus one.
- Block head + k gets the EVM timestamp `floor((head.timestamp_ms + k·block_time_ms) / 1000)`,
  in whole seconds as on chain.
- Under Denim, a head without `timestampMs` gets no block context, and Tier 2
  doesn't simulate on it. There is no fallback clock: a whole-second
  timestamp doesn't say where in its second the head is, so a fallback could
  be a second early.

Tier 2 is the only user of a synthesised clock, and it is parked. The armed
lane needs none:
- its calldata deadline is wall-clock seconds, and covers the window;
- the evaluator replays real blocks under their own headers.

`Head` gains an optional `timestamp_ms`, parsed from `timestampMs`. A test
records every request the runtime makes under a Denim config and asserts no
`"pending"` appears.

### 5. Feed, capacity and cadence under Denim

- **The feed.** The Denim config doesn't subscribe to `pendingLogs` (R36's
  switch), so BlockPI's answer to it doesn't matter. The `ws.rs` rule that a
  refused subscription ends the session stays: under Denim no subscription
  should be refused.
- **No flashblocks section, so no capacity sampling (my call).** The
  capacity model is a flashblock model. Without it the parked reactive lane
  refuses every submission, which is the fail-safe it already has.
- **Under Denim, the guard is three signals, all from `timestampMs`.**
  Pre-fork configs keep today's median of arrival intervals, unchanged.
  - **Chain cadence:** the newest head's `timestampMs` minus the oldest's,
    divided by the blocks between them, over the last 20 heads, within half
    of `block_time_ms` either way. This is the chain's clock, not delivery's:
    - a burst of arrivals can't move it;
    - neither can a skipped head;
    - on Sepolia it is exactly 200.
  - **Head lag:** at every send, now minus the newest head's `timestampMs`
    must be no more than `armed.max_head_lag_ms` (1,000, five blocks;
    **my call**).
    - A feed that delivers late, or stops, stops the sends within a second.
      Today the only cue is the stops going stale after 6 s.
    - Measured from NZ, p99 was 347 ms and the maximum 487.
    - The host's clock is chrony on Amazon Time Sync. Gate S checks its
      offset.
  - **No `timestampMs`, no sends.** A Denim head without the field passes
    neither check.
- **Continuity (my call).**
  - A skipped head number changes nothing that matters:
    - the book's logs are their own subscription;
    - a reconnect or a dropped notification already rebuilds the book;
    - settlement compares bounds with `≥`.
  - A repeated head is already ignored (`on_head` takes only `head > last`).
  - Skips are counted in the report for gate M.
  - Parent hashes are not tracked: Base has one sequencer, and a reorg
    arrives as removed logs, which reload their pools.
- **Tests:**
  - Sepolia's measured heads pass at 200 configured and fail at 2,000.
  - The same heads arriving as a burst of five within 10 ms still pass.
  - A skipped head passes.
  - A newest head 1,001 ms old refuses a send, and 999 ms allows one.
  - A Denim head without `timestampMs` refuses a send.
  - A pre-fork config is decided by today's median exactly as before.

### 6. The evaluator in blocks

- **`SETTLE_BLOCKS` becomes a minute in the config's blocks:** 30 at 2 s,
  300 at 200 ms. The direction across the fork is safe: pre-fork arms judged
  after the switch have long settled.
- **A block's last transaction, in block mode.**
  - With no flashblock ledger, the evaluator reads each fire block's
    transaction count, once per block and cached
    (`eth_getBlockTransactionCountByNumber`, sampled with the fires).
  - If the fire's transaction was the block's last, ours lands in the next
    block at the earliest, where the contest is by priority fee.
  - Positions in block mode are blocks. "Next" is the next block, not
    `next_flashblock((block, 0))`.
  - Tests:
    - the trigger closes its block, and a rival in the next block outbids us
      → lost;
    - the same with our bid higher → won;
    - the trigger doesn't close its block, and a rival in the same block bids
      more → lost;
    - a rival in a later block → won.
- **Order inside a block is already kept.** `judge` runs effects in
  `(block, tx_index)` order, and the rival is the first transaction after
  the fire that breaks a predicate. A transaction before the trigger can
  never be the rival. Block mode changes how positions are compared, not
  which transactions count. New tests pin what isn't pinned today:
  - a write before the fire in the same block is never the rival;
  - of two rivals after the fire, the first decides;
  - a rival after the trigger in its own block is a fee contest;
  - our own landing, in block mode, wins at the bid sent and above.
- **The start point is the book's, not the head's.**
  - An arm's start words come from the book. The book can already hold
    writes from blocks after `armed_block`, the head at arming: today from
    `pendingLogs`, and on Denim from `logs` that arrive before their head.
  - `judge` replays every write after `armed_block` on top of those words.
    A price or reserve word is set absolutely, so replaying it changes
    nothing. A Mint or Burn moves liquidity by a delta, so it is counted
    twice.
  - Liquidity has a floor predicate, so a burn counted twice can invent a
    rival.
  - **The fix:** each arm records its two pools' applied-through points
    (`start_at`). That is the pool's last applied log (block, log index), or
    the block its state was read at. `judge` replays a pool's writes only
    after that point, which needs each write's log index in `TxEffect`.
  - Arm lines without `start_at` (every arm before the deploy) are judged
    exactly as today.
  - **Test:** an arm whose start words already include a burn at
    `(armed_block + 1, log 3)` judges the same as an arm taken after it.
    Without `start_at`, it gives today's answer.
  - It ships with the pre-fork deploy, so that every post-fork arm carries
    its start point.
- **Verdicts gain an `offset_unit`**, one of three:
  - `flashblock`: the ledger placed both;
  - `block_2s`: a pre-fork block with no ledger, which happens today in
    ledger gaps;
  - `block_200ms`: a fire at or after the fork block.

  R38's contest tooling then counts each separately, and never pools them as
  one.

### 7. The census

- **The trigger window becomes 300 s.** That is 150 blocks before the fork
  block and 1,500 from it. The fork block is found once by timestamp search
  (`_first_block_at_or_after(..., 1793901600)`) and cached in `meta.json`.
- **The fork day (11-05) stays as the code does it today:** its flashblock
  ledger ranges 00:00–18:00 and the last six hours are not counted
  **(my call)**. That day is never mixed across units.
- **Every census day records its `offset_unit`,** with §6's three values.
  - `targets.py` ranks routes from `flashblock` and `block_200ms` days
    together, because an offset of 0 means the trigger's own ~200 ms unit in
    both.
  - A `block_2s` day counts for route value, but not for offsets.
  - `contest.py` keeps all three apart.
- **Run cost.** The builder measures today's daily run: wall time, and calls
  by method from the journal. They report the estimate for a Denim day, with
  10× the `getLogs` windows at `STEP = 50` and receipts unchanged. If it
  would exceed six hours, that goes back to the operator before deploy,
  since the job runs at 00:20 and the canary's targets expire after two days.

### 8. `apex denim-check`: the rehearsal and the gate

A subcommand that runs the real parts against any chain for `--for-s`
seconds and prints one JSON verdict:
- the capture feed with `subscriptions(config.feed, filters)`, answer by
  answer;
- heads and logs counted, with skips and repeats;
- the guard's own functions over the heads: chain cadence and head lag
  (§5), with lag p50, p99 and max;
- every head's `timestampMs` checked against the lattice, as the switch
  does (§2);
- the head-read clock (§3) driving a counting stub;
- websocket bytes by subscription.

It passes when:
- every subscription is accepted;
- after a 20-head warm-up, the guard holds for at least 99% of heads;
- lag p99 is within `max_head_lag_ms`;
- read rounds come no faster than one per `head_reads_every_ms`.

It reads no keys when the config's URLs carry none, and accepts any chain
ID, which it prints. Gate S runs it on Sepolia with a scratch copy of
`canary.denim.yaml` pointed at public endpoints. Gate M runs it on mainnet
against BlockPI.

### 9. The runbook

Rewrite `ops/host/ARMED.md` "The Denim day" as:
- the switch, and how to read its record;
- the manual fallback (§Rollback);
- gate M's checks.

`ops/host/README.md` gains the timer, the drop-ins, and a warning: do not
archive `var/apex` between now and 11-07, because doing so removes
`canary.stop` and the flashblock ledgers that the census relies on.

## Gates and timeline

| When | Gate | Passes when |
|---|---|---|
| 10-31 | Spec approved | The operator's decisions below. |
| 11-01–02 | **B**: build, on branch `r41-denim` | All five CI gates, plus the tests in §1–7. The mutation pass kills every mutant in the new guards: the config check, the read clock, stops freshness, chain cadence, head lag, the block-mode landing, `start_at`, and the switch's lattice checks. |
| 11-02 | **S**: Sepolia rehearsal | `denim-check` for 10 minutes passes on public Sepolia. `denim-switch.sh --dry-run` detects Denim on Sepolia and "not yet" on mainnet. The drift test has loaded all four configs through the real loader. |
| 11-03 | Deploy | The operator's "yes, push and deploy <rev>". Restart without archiving. The status lines match the pre-deploy run: same read rates, the guard holds, arms sent. `systemctl list-timers` shows the switch at 11-05 17:59:00 UTC. The report shows bytes by method: that is the baseline for gate M. |
| 11-04 | Rehearsal on the host | The switch in `--dry-run` against mainnet says "not yet", and against Sepolia it prints the drop-ins it would write. `chronyc tracking` shows the host within 10 ms. The latest stable `base/base` release still has Denim at 1_793_901_600; if it has moved, re-arm the timer for the new date. |
| 11-05 ~12:00 | Last look | The same release check, and Base's status page. |
| 11-05 17:59 | The switch | Detects within 10 s of the first Denim block, and restarts. |
| 11-05 18:00–19:00 | **M**: mainnet | See below. Read by the operator or me at about 18:30 (07:30 NZDT). |
| 11-06–07 | **D**: a day on Denim | Below. |

**Gate M** (from `shadow-status.sh`, the report and `denim-check` against
BlockPI):
- **The switch.** Its record shows the ten lattice blocks and the restart.
- **The cadence.** Chain cadence is 200 and the guard holds. Head lag p99
  is under 1,000 ms; record p50 and p99, since they are BlockPI's delivery
  on Denim. Skips are counted. Stops are under 6 s old. Read rounds run at
  no more than 0.5 a second.
- **The book.** It is not Rebuilding, logs arrive every minute, and there is
  no reconnect storm. (This is a check by eye. What stops a stale book from
  trading is the predicates, §Facts.)
- **The first arms.** They carry bound = head + 50 and `start_at`, and are
  accepted. Any landing reconciles in the ledger from its receipt.
- **BlockPI.** Bytes per hour by method are within 1.5× of the 11-03
  baseline.
- **Responses:**
  - **Refusals naming the bound:** set `window_s` to 6 (30 blocks) in the
    Denim config and restart.
  - **BlockPI frozen or storming:** touch `armed.disable` and test the key
    over HTTP for the error code. Then decision 4's fallback, or wait.
  - **Lag p99 over 1,000 ms while BlockPI is otherwise healthy:** the guard
    is idling the lane on delivery. Raise `max_head_lag_ms` (up to 5,000)
    only if the lag is steady rather than growing.
  - **RU rising:** stop the canary and read the per-method counters.

**Gate D:**
- The evaluator judges post-fork arms with `offset_unit: block_200ms`, and
  the trigger-closes-its-block case appears.
- Targets for 11-06 are written, and the run's time and calls are recorded.
- BlockPI's daily usage is no higher than before the fork.
- R38's contest tooling sees the two eras apart.

## Rollback

- **After the fork, there is no going back to 2 s.**
  - The kill file (`var/armed/armed.disable`) stops sends at once, and the
    process keeps measuring.
  - To stop spending RU, stop `apex-canary`.
- **If R41's build or deploy slips past 11-05 12:00:**
  - the operator flips R36's three fields at 07:00 NZDT, as today's
    `ARMED.md` says, and touches the kill file;
  - without §3, the old binary on a Denim config reads every head, at 10×
    the rate, so it is safe only for hours;
  - after the operator's first check, stop the canary until R41 deploys.

## Risks

- **BlockPI misses the upgrade.** Heads freeze. The switch still proceeds,
  because the public RPC shows the fork, and the head-lag check idles the
  lane within a second. Decision 4.
- **BlockPI's heads lack `timestampMs`.** The Denim guard refuses every send.
  The lane is idle, not wrong. Gate M sees it at once, and the fix is
  provider-side or a second websocket (decision 4).
- **BlockPI rejects `pending`.** §4 removes every use under Denim.
  Pre-fork, nothing changes.
- **Bound semantics.** If the sequencer measures the 60 s in 2 s blocks,
  50-block bounds are refused. The response is config only (Gate M).
- **Fees.** On Sepolia's parameters the base fee can rise 1.4% a block, so
  2× over a 50-block window. The lane's `max_fee = 2 × base + priority`
  stops covering it near the window's end in a sustained surge. That means
  non-inclusion, not a loss. Mainnet's parameters are unknown until the fork.
- **Slipstream's dynamic fee** charges 150 ppm on a pool's first swap in a
  block. With five blocks a second, an arm that lands as a block's first swap
  on its pool is more common. The executor's floor protects the money. The
  evaluator's simulation-agreement rate will show it.
- **The rival contest changes.** Pre-Denim bid fits describe flashblock-era
  contests. R40.1's decision 6 already holds `best_bid` through Denim and
  refits on post-Denim days (11-10 at the earliest).
- **The census runs slow or fails.** Targets expire two days after the last
  good run, and only a restart reads them. §7 measures the cost before the
  deploy, and gate D watches the first runs.

## What is left to the builder

- The exact shape of `denim-check`'s JSON and of the drift test's YAML
  comparison.
- How the read clock is held: an `Instant` in `head_loop`, and the same in
  the armed live worker.
- Where `timestamp_ms` enters `Head`: `apex-chain` parses headers. Then how
  it reaches `on_head`, beside the head number it takes today.
- `start_at`'s field shape on the arm line, and where `effects_of` takes each
  log's index.
- The drop-in contents. Each needs the empty `ExecStart=` line before the new
  one. Check that `provision.sh` and `deploy.sh` leave `*.service.d/` alone.
- Whether the moves seed (`apex armed-moves-seed`) should range by timestamp.
  Until it does, `ARMED.md` says not to run it across the fork. `moves.json`
  persists across restarts, so the switch doesn't need it.

## Relation to R40.1 and R38

- **R41 first.** R40.1's implementation plan follows once R41 is deployed.
  Its gates C and L, and its tier-2 threshold, are fitted on post-Denim days.
- **R38.** Its 11-02 gates are measured on flashblock-era contests. Under
  R40.1's decision 6, `best_bid` is held through the fork, and the refit uses
  `offset_unit: block_200ms` rows only.

## Review response

Each point of the 2026-10-31 review, checked against the code and against
Base's v1.5.1 client source before answering.

| Point | Verdict | What changed, and why |
|---|---|---|
| **P0-1** Keep the order inside a block in the evaluator | **Already so, and tests added** | `judge` orders by `(block, tx_index)`, and the rival is the first transaction after the fire, so one before the trigger can never count. Positions carry the transaction index. §6 adds the four tests that weren't there. Checking this found a real fault next door: the start words can already hold writes after `armed_block`, and a replayed Mint or Burn counts twice. Fixed by `start_at` (§6). |
| **P0-2** Separate chain cadence, delivery lag and continuity | **Accepted, modified** | The Denim guard is now chain cadence from `timestampMs` plus head lag at every send (§5). This replaces my window-rate guard, which still measured delivery. Continuity: skips and repeats are counted, but don't gate sends, and parent hashes are not tracked. Neither affects the book or settlement (§5); a reconnect, a dropped notification or a reorg already rebuilds or reloads the book. |
| **P0-3** No approximate millisecond clock | **Accepted** | The fallback clock is gone. Under Denim a head without `timestampMs` gets no block context and no sends (§4, §5). EVM time stays in whole seconds. Only the parked Tier 2 ever synthesised a clock. |
| **P0-4** Prove the book has caught up before arming | **Already so on chain, plus §6** | The predicates pin the other pool's band and liquidity, and the trigger past its threshold. The builder checks them at execution, so an arm priced on a book the chain has left is not included and pays nothing. The feed already rebuilds on a reconnect or a dropped notification, reloads on a reorg, and refuses out-of-order logs per pool. A client-side "caught up" gate would duplicate what the chain enforces exactly. The "last write under 10 s" line was a gate-M check by eye, not a send gate; it is reworded. |
| **P0-5** Prove the fork, not infer it | **Accepted, one part modified** | The detector requires all of: chain ID 8453, ten consecutive blocks on the exact 200 ms lattice, and every one at or after 1_793_901_600. That value is now verified in the client source, not only the release notes. Re-checked 11-04 and 11-05. A second public source is a fallback, not a co-signer (my call): a false switch idles the lane anyway, so requiring two only adds a way to fail. |
| **P1-1** Don't rely on BlockPI alone | **Not for the fork; costed and dated** | At R39's ~$1.73/day, an outage costs cents an hour and loses nothing. A day spent on failover before the fork buys less than the remaining work does. Revisit with R40.1. The absence of alerting is stated (decision 4). |
| **P1-2** Measure RU in bytes; a hard ceiling; jobs must not starve the lane | **Bytes accepted; ceiling declined; starvation already so** | The report already has bytes by method (R28.1) and gains bytes by subscription (§3), and gate M compares bytes against an 11-03 baseline. The census uses public endpoints, never BlockPI. The evaluator's Denim cost is about 4 calls a run (§3). A self-imposed RU ceiling would idle the lane exactly as exhaustion does, only sooner. What bounds the spend is the read clock's rate, which is tested, mutation-checked and checked at gate M. |
| **P1-3** Keep execution-critical data out of the 2 s cache | **Already so; stated** | Every read on the clock is slow-moving (§3). Prices, liquidity and Slipstream's tick term come from the log feed every block and are checked on chain by the predicates. The wall-clock freshness of each read is the same as today. |
| **P1-4** Readiness is not profitability; change nothing else on the fork day | **Accepted** | Two anti-goals added: no bid, tier or funding change at the fork, and R41's gates are not evidence of profit. |
| Decision 3: kill file instead of live | **Kept live; trade-off costed** | The conditions the review asks for before sends resume are now automatic, and re-checked on every send (§2). The kill file stays available. Decision 3 now carries both sides' cost. |
| Decision 5, 6 | **Agreed** | Unchanged. |

**Timeline effect.** `start_at`, the bytes counters, the lattice checks and
the lag check add about half a day to gate B. The chain-cadence guard
replaces the window-rate guard rather than adding to it. B stays at 11-01–02.

## Decisions that are the operator's

1. **Approve R41's scope** as written: §1–9, with the reactive lane parked.
2. **The switch at the fork is automatic** (recommended): detected on chain,
   and it restarts the canary at about 18:00:10 with no one awake. The
   alternative is a manual flip at 07:00 NZDT. Approving the automatic switch
   pre-authorises that one restart.
3. **The armed lane stays live through the fork** (recommended).
   - The new process sends only once the five conditions in §2 hold, and
     re-checks them before every send. The 0.01 ETH daily budget bounds the
     first Denim hour.
   - The realistic failure is an arm that matches nothing, which pays
     nothing, or an included revert at about a cent.
   - The alternative is the kill file from 17:55 until someone reads gate M
     at about 07:30 NZDT. At R39's measured ~$1.73/day that forgoes about
     4 cents. The real cost is the first half hour of post-fork contests.
   - My guess, not measured: that half hour may be unusually open, because
     any rival that relied on flashblocks or `pendingLogs` is blind until it
     migrates.
4. **Websocket failover.** Accept BlockPI as the single websocket through the
   fork (recommended), or add a second provider's websocket with failover.
   - Failover costs about a day of work, and a key placed in the host's env
     as an `APEX_SECRET_*` variable.
   - At R39's ~$1.73/day, a whole day's outage forgoes about $1.73, and an
     outage is idle, not a loss.
   - It is worth revisiting with R40.1, once the lane earns enough for an
     hour idle to matter.
   - The host has no alerting, so an outage before 07:30 NZDT goes unseen
     until gate M.
5. **Deploy on 11-03, restart without archiving**, so `canary.stop`, the
   flashblock ledgers and the 14-day records stay in place.
6. **A BlockPI Base Sepolia key** (optional). Gate S would then rehearse
   against BlockPI's own Denim node rather than PublicNode's. This is the
   only pre-fork way to learn BlockPI's answer to `pendingLogs`, `pending`,
   and its head delivery.
