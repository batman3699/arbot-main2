# A co-located shadow and a race ledger (Task 8.5 R25)

Approved by the operator 2026-10-11. Step 1 of the profit path
(https://claude.ai/artifact/Q6eRQSQZ8DdEqWpDfHdW34).

## Goal

Run the shadow next to Base's sequencer, and measure the race it is in. For
every gap the shadow finds, we need to know:

- which flashblock opened it;
- which flashblock our transaction would have landed in, both at proposal
  time and after Tier 2;
- which flashblock the winning rival landed in.

Step 1 does not change what is traded. It removes the network distance and
measures exactly what step 2, the fast submission path, must remove.

## Evidence

Measured 2026-10-10 and -11, from the laptop in New Zealand:

- **We lose by about a flashblock.** Since R21 the shadow found $301 of
  expected value; $6.88 passed Tier 2. Of 28 failed tickets worth $0.50 or
  more, a rival's arbitrage of exactly our route's two pools landed:
  - in the block we detected in: 18;
  - in the next block: 1;
  - in none of the next four blocks: 9.

  In the largest case ($192, block 52,342,205) the trigger is transaction
  index 303 and rivals hold indexes 304–308, at priority fees of 0.2–1.8 gwei
  against a 0.005 gwei base fee.
- **Network.** From the laptop, Cloudflare's edge (`colo=WLG`) answers in
  ~30 ms, and an `eth_blockNumber` to `mainnet.base.org` takes 222 ms. That
  puts the origin ~190 ms beyond the edge, consistent with the US East coast.
  BlockPI's keyed RPC is 195 ms at best.
- **Flashblocks.** Base's public stream, `wss://mainnet.flashblocks.base.org/ws`:
  - Frames are brotli-compressed JSON:
    - `payload_id`, `index`;
    - on index 0 only, `base` with the block number and timestamp;
    - `diff`: `transactions` (raw), `gas_used`, roots and withdrawals;
    - `metadata`: `block_number`, `prev_flashblock_id`.
  - It carries **no receipts or logs**.
  - There are 11 flashblocks per block (indexes 0–10), about 170 ms apart.
  - Index 0 reaches the laptop ~128 ms after its slot opens (block timestamp
    − 2 s).
- **BlockPI's `pendingLogs` trails the raw flashblock** carrying the same
  transaction: matched by transaction index on 547 swaps, by +6 ms at p10,
  +45 ms at p50 and +92 ms at p90.
- **Tier 2** takes ~230 ms (R18, R21).

The arithmetic: landing in the flashblock right after the trigger leaves
~150 ms from the trigger's publication to our transaction reaching the
sequencer. From the laptop, one round trip uses all of it, so co-location is
the precondition. Tier 2 alone also exceeds the budget, so step 2 must take it
off the critical path. This step measures both.

## What changes

### 1. A region probe: `apex probe --minutes N`

A new subcommand that runs no trading component. It runs the flashblock
reader (§4), a `pendingLogs` subscription to BlockPI, and timed round trips,
then prints:

- **arrival offset**: how long after its slot opens (block timestamp − 2 s)
  each index-0 flashblock arrives, at p10, p50 and p90. This is the distance
  to the builder.
- **round trips**, at p50 and min:
  - to Base's RPC origin (`eth_blockNumber` on `mainnet.base.org`) and its
    Cloudflare edge (`/cdn-cgi/trace`);
  - to the sequencer's endpoint (`eth_chainId` on
    `mainnet-sequencer.base.org`);
  - to BlockPI.
- **BlockPI's delay**: `pendingLogs` arrival minus the raw flashblock carrying
  the same transaction index, at p10, p50 and p90.

The operator launches one small instance in each of us-east-1 and us-east-2
and runs it for 60 minutes in each. The lower p50 arrival offset wins, and
BlockPI's p90 delay breaks a tie within 5 ms. Half the winner's round trip to
the sequencer's endpoint is **δ**, the send delay that §6 uses.

### 2. The host

- AWS EC2 **c7i.xlarge** (4 vCPU, 8 GB, ≈ $0.18 an hour on demand, ≈ $130 a
  month) in the winning region.
- Ubuntu 24.04 LTS, a 50 GB encrypted gp3 volume, a 4 GB swap file, and
  **IMDSv2 required**.
- **Access: SSM Session Manager only.** The instance profile carries
  `AmazonSSMManagedInstanceCore`. The security group allows **no inbound**
  traffic, and outbound only 443 (and 80 for apt).
- **Clock:** chrony against Amazon Time Sync (169.254.169.123). Every
  comparison is between two readings of this host's clock, except arrival
  offsets, which also need the block timestamp.
- **Build on the host**, from `git archive` of a pinned, committed revision:
  - the toolchain is `rust-toolchain.toml`'s (1.97.0);
  - the build uses thin LTO (`CARGO_PROFILE_RELEASE_LTO=thin
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`) and `-j 3`;
  - the deploy script writes the revision to `/opt/apex/REVISION`, and the
    status script prints it.
- **Data:** `data/base` (outside git) is copied with a sha256 manifest,
  verified on the host before the first start.
- **Service:** a systemd unit, `apex-shadow`:
  - runs `apex shadow --config ops/shadow.base.yaml` as user `apex` from
    `/opt/apex`;
  - `Restart=on-failure`, `RestartSec=10`, `KillSignal=SIGINT`,
    `TimeoutStopSec=90` (the drain);
  - hardened with `NoNewPrivileges`, `ProtectSystem=strict`,
    `ReadWritePaths=/opt/apex/var` and `PrivateTmp`.

  This also ends the laptop's crashes and "nothing restarts it after a
  reboot". A restart on the same revision continues the same run, as on the
  laptop.

### 3. The signing key (operator decision: reuse trader `0xCB43…`)

- **Where it lives:** `/etc/apex/shadow.env`, mode 0640, `root:apex`, holding
  `APEX_SECRET_BLOCKPI_KEY` and `APEX_SECRET_TRADER_KEY`. It is loaded by the
  unit's `EnvironmentFile`. It is never in argv, logs, the repository, or any
  snapshot or AMI. `scripts/shadow.sh`'s `.env` mapping is not used on the
  host.
- **Exposure:** the trader holds only the 0.001 ETH gas reserve, and the
  executor holds no capital between trades.
- **Tripwire:** a shadow never broadcasts, so the trader's nonce cannot move.
  The head loop reads it with the reader's view. If it differs from the boot
  value, the shadow logs an `ERROR` every head and the report carries
  `signer.nonce_moved: true`.
- **Revocation:** the owner's `setExecutor(0xCB43…, false)` is prepared as
  calldata and dry-run against Base before the host starts. The operator
  broadcasts it if the tripwire fires.

### 4. The flashblock reader (code, `apex-chain`)

- A reader for Base's public flashblock stream. It brotli-decompresses each
  frame, parses the fields above, and yields one value per frame:
  `FlashblockSeen { block, index, arrived_at: UnixNanos, tx_range: (first,
  last), block_timestamp: Option<u64> }`.
  - `tx_range` is the cumulative transaction-index range this flashblock added
    to its block.
  - `block_timestamp` comes from index 0's `base`.
- A new shadow worker writes these to `var/apex/flashblocks.jsonl`, rotated
  daily, and keeps the last 64 blocks in memory for §5.
- **Measurement only:** nothing priced or signed reads it. A dropped session
  reconnects with backoff. A frame that does not decode is counted, never
  guessed. A block whose index sequence has a hole is marked incomplete, and
  §6 leaves it out.
- The report gains `flashblocks: { sessions, frames, decode_failures,
  incomplete_blocks }`.

### 5. The race ledger (code, `apex-runtime`)

- `RawLog` gains `transaction_index`, parsed from `pendingLogs`.
- For every Engine C proposal and every ticket, one line goes to
  `var/apex/race.jsonl`:
  - `route_hash`, `ticket_id` (if one was admitted), `net_hint` and the
    outcome (declined, stale, simulated, reverted with its class, or
    dispatched);
  - `trigger`: the event's last log, as `{ block, tx_index, log_index, tx }`,
    and its flashblock *k* from §4's index by `tx_index` (null if §4 has not
    seen that block);
  - `t`: host-clock nanoseconds at each stage:
    - `flashblock_k`: §4's arrival of the flashblock carrying the trigger;
    - `observed`: the event's `observed_at`, when the burst's last log
      arrived;
    - `published`: the burst's flush, after `BURST_QUIET`;
    - `proposed`;
    - `admitted`;
    - `tier2_start` and `tier2_end`;
    - `would_send`: signed, or when the outcome was decided.
- Lines are appended by one writer task over a bounded channel. A full
  channel drops the line and counts the drop (`race.dropped`), and never
  blocks the capture path.

### 6. The contest analyzer (`scripts/data/race_analysis.py`)

It reads `race.jsonl` and `flashblocks.jsonl`, plus on-chain logs.

- **Would-land flashblock** at time *t*: the first flashblock whose arrival
  at the host is ≥ *t* + δ + ε. δ is from §1. ε = 20 ms is the sequencer's
  seal margin: an assumption, stated as one, which the live pilot calibrates.
  It is computed at `proposed` (as step 2 would send) and at `tier2_end` (as
  today).
- **Rival:** the first transaction after the trigger that swaps both of the
  route's pools, within 4 blocks. Its flashblock comes from §4's ranges by its
  transaction index.
- **A contested gap** is a route proposal or ticket with such a rival.
- **Output:** one row per contested gap, giving:
  - the trigger flashblock *k*, the rival's flashblock, and ours at proposal
    and after Tier 2, each as an offset from *k*;
  - the stage latencies;
  - the rival's priority fee.

  It also reports the distributions.

### 7. Gates, over 7 days on the host

- **G1, detection:** from `flashblock_k` to `published`, **p50 ≤ 80 ms and
  p90 ≤ 130 ms**. BlockPI's measured 45 ms and 92 ms, plus `BURST_QUIET`'s
  20 ms, plus 15 ms of margin. Passing shows the network floor is gone. (The
  design note said 60 and 120 ms. Those would fail by construction, because
  publication includes `BURST_QUIET`. Corrected here.)
- **G2, whether step 2 can work:** at `proposed`, the would-land flashblock is
  **≤ *k* + 1 on at least 50%** of contested gaps.
- **Then:**
  - If G2 passes, step 2 (the fast path and priority-fee bidding) is specified
    next.
  - If G2 fails, and subtracting BlockPI's measured delay from each gap would
    pass it, the next spec is our own flashblocks-aware node.
  - Otherwise our own pipeline is the cost, and its stage latencies say where.

The 14-day clock restarts on the host.

## What does not change

- Trading logic, venues, sizing, the risk gate and the executor.
- Nothing is broadcast: the null dispatcher stays the only lane.
- The laptop stays the development box. Its shadow stays paused, and the host
  run is the measured one.
- BlockPI stays the RPC. A second provider for failover is left for later.

## Verification

1. **Unit tests:**
   - the flashblock reader on recorded frames (brotli, index 0 with `base`,
     the others without), and a hole marking a block incomplete;
   - `transaction_index` parsed from a recorded `pendingLogs` entry;
   - the race-ledger line's fields and its never-blocking drop;
   - the tripwire on a moved nonce;
   - the analyzer on a recorded block (the ticket-263 shape: trigger at 303,
     rivals at 304–308).
2. **The probe** on the laptop first. It must reproduce this spec's laptop
   figures within noise: arrival offset ~128 ms, BlockPI delay ~45 ms at p50.
3. **The host:**
   - boots with the configured revision and verified data;
   - the unit survives a `kill -9` (restarts) and a reboot;
   - `systemctl stop` drains;
   - the secrets file's mode and owner are checked;
   - no inbound port answers.
4. **After the first hour:** the ledgers are non-empty, every race line's
   `flashblock_k` resolves for blocks §4 saw whole, and G1's figures are
   reported.

## Risks

- **Reusing the trader key on a cloud host.** Mitigations: SSM-only access,
  no inbound ports, IMDSv2, an encrypted volume, a root-only secrets file,
  gas-only funding, the nonce tripwire, and a prepared revocation.
- **The public flashblock stream** is unauthenticated and its format can
  change. The reader is measurement-only and counts what it cannot decode.
- **BlockPI is a single provider.** An outage stops the shadow as it does
  today, and the systemd restart recovers it once BlockPI answers.
- **ε is assumed** until the pilot measures it. The analyzer reports G2 at ε
  of 0, 20 and 40 ms, so the gate's sensitivity shows.
- **Cost:** about $130 a month for the host, plus cents for the probe
  instances.

## Rollout

1. Build and test §1, §4, §5 and §6 on the laptop. Run the probe locally.
   Commit.
2. The operator creates the two probe instances, with the instance profile,
   the security group and IMDSv2. I provide the exact settings and scripts,
   and the operator runs them (or grants a scoped IAM user for `ssm
   send-command`). Run the probe and choose the region.
3. Launch the host in that region and deploy the pinned revision and data.
   Write the secrets file. Dry-run the revocation. Start the unit.
4. After 7 days, run the analyzer and decide by §7.
