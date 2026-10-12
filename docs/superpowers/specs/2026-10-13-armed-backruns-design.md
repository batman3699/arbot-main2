# Armed backruns through Base validity transactions (Task 8.5 R26)

Approved by the operator 2026-10-13: approach 1 (arm inside the shadow,
judge with a separate exact replay tool); scope "shadow, then a gated pilot";
pilot loss cap US$25 per UTC day.

## Goal

Stop racing the builder from outside it. Since Cobalt (Base mainnet,
2026-09-30), a signed transaction can wait inside Base's builder with
conditions on chain state. It becomes eligible the moment an earlier
transaction makes them true, in the same flashblock. A bot that arms the
gap's condition in advance needs no detection, no reaction and no network
path at the moment of the trigger.

R26 builds one arming engine with two modes:

- **armed-shadow:** arm, sign and record, never send; a replay tool judges
  every arm against the chain: would it have fired, won and earned, and at
  which bid;
- **armed-live:** the same arms, sent with `base_sendRawTransactionValidity`,
  under hard caps, switched on only after gate A and the operator's
  approval.

## Evidence

- **The race we lose.** Of 28 failed tickets worth $0.50 or more
  (2026-10-07 to -09), a rival's arbitrage of our route's two pools
  landed in the block we detected in 18 times. In the largest ($192,
  ticket 263) the trigger is transaction 303 and the rivals are 304–308.
  Both postdate Cobalt.
- **Co-location narrows the gap but cannot close it** (2026-10-12, 60 min,
  c7i.xlarge us-east-1): a flashblock arrives 36 ms (p50) after its slot
  opens; BlockPI's logs trail it by 35 ms (p50); the sequencer is 15 ms
  away; detection on the host measured 39 ms on the first contested gap. A
  reactive transaction lands one flashblock after its trigger at best. An
  armed one can land right after it.
- **Validity transactions** (docs.base.org, read 2026-10-12):
  - `base_sendRawTransactionValidity(rawTx, {validity: [...]})`, on the
    sequencer endpoint `https://mainnet-sequencer.base.org` only;
  - predicates `balance`, `storage` (address, slot, optional mask, `op`,
    value), `block_number`, `flashblock_index` (1–10); operators `<`, `<=`,
    `=`, `!=`, `>`, `>=`; 0x-hex values; raw storage only;
  - evaluated against the state immediately before the transaction would
    execute; a transaction can become eligible in the same flashblock as
    the transaction that changed the watched slot;
  - not included, no fee; included, normal fees, even if it reverts;
  - eligible transactions compete by priority fee; eligibility reserves no
    space;
  - every transaction needs a `block_number` upper bound within 60 s of
    submission;
  - replacement by same sender and nonce, and a fee bump if the RPC says
    underpriced;
  - criteria are private and never recorded on chain.
- **Pool storage, verified on chain** at blocks 52,504,197 and 52,509,567
  against the pools' own getters:

  | Venue | Price word | Liquidity |
  |---|---|---|
  | Uniswap v3 | slot 0, low 160 bits = sqrtPriceX96, bits 160–183 = tick | slot 4, low 128 bits |
  | PancakeSwap v3 | slot 0, same packing | **slot 5**, low 128 bits |
  | Slipstream (both factories) | **slot 6**, same packing | **slot 16**, low 128 bits |
  | Aerodrome v2 | reserve0 slot 20, reserve1 slot 21 (full words) | — |

- **The executor needs no change.** `startV2` takes a plan with a deadline
  (seconds), `minProfit`, per-step `minOut` (non-zero) and a commitment.
  It emits `ProfitRealised(token, grossProfit, ownerFee)`.

## What changes

### 1. `move_to_price` (code, `apex-math`)

The exact state of a pool after the price moves to a target.

- **Concentrated liquidity:** steps the tick ladder (`TickLadder::next_initialized`)
  from the current state to the target sqrtPriceX96, crossing each
  initialized tick with its `liquidityNet`. It returns `(sqrtPriceX96,
  tick, liquidity)`. A target outside the ladder's proven range returns
  `None`, and the slot is not armed.
- **Aerodrome v2:** `reserve0' = isqrt(k / p)`, `reserve1' = isqrt(k · p)`
  with `k = reserve0 · reserve1`, rounded against us.

The state at a price does not depend on the path taken to it (same
liquidity distribution), so an arm stays valid while its trigger pool
drifts without reaching the threshold.

### 2. The arming engine (code, `apex-runtime::armed`, a worker in the shadow)

It runs as its own supervised worker, beside the capture loop. It reads
the book through its `Arc`, recomputes at most every 200 ms on a book
change, and never blocks capture.

**Slots.** A slot is a cycle (`frontier::Cycle`, already directional) and
its trigger pool, one of its two pools. The move that opens the cycle's
gap is fixed by the cycle: its first leg's token gets cheaper on the
first pool, or dearer on the second. So there are two slots per cycle, and
316 over today's 158 cycles.

**Threshold.** For each slot, a binary search finds `m₁`, the smallest move of
the trigger pool, in ticks for concentrated liquidity or in price for
Aerodrome v2. The target is a move at which the cycle, priced by Engine C's
sizing (`LivePricer::best_size`, exposed to the crate), clears the hurdle
`H = base gas + L1 fee + margin`. The pricing uses `move_to_price` for the
trigger pool and the book's state for the other. Profit at a fixed size
grows with the move, so the search is monotonic. A slot with no such
move inside the ladder is not armed.

**Rungs.** Each slot is armed at up to three moves: the threshold `m₁`,
then `m₂ = 2·m₁` and `m₃ = 4·m₁` (`rungs: 3`). Each rung has its own size,
its own bid and its own sender, all set at its own threshold. A bid fixed
at break-even would be close to nothing and lose every same-flashblock
race. On a large move every rung up to it becomes eligible, and the
builder executes the highest-bidding one first: the largest. A lower rung
whose predicates still hold afterwards still clears its own threshold, so
it is still profitable.

**Ranking.** The engine keeps the best N (slot, rung) pairs armed. The
score is `P(move ≥ m_r within W) × expected net at m_r`. The net is taken
before the bid in armed-shadow, where no `f` is bound, and after gate A's
bid in armed-live. `P` is learned online, per pool and direction, from the
feed's swap history: the distribution of the largest move over every
W-second window.

**The plan.**
- **Size** `s*`: the best size at the threshold state.
- **Floors are break-even, not the hurdle:** hop 1 `minOut = 1`, hop 2
  `minOut = loan + 1`, `minProfit = 1`. Once included, the fee is spent
  either way; a thin trade that executes earns more than a revert. The
  predicates carry the profitability before inclusion. The reactive path's
  tight floors stay as they are.
- **Gas limit:** from the measured per-crossing model (R12) at the
  threshold state, plus a 25% margin.
- **Deadline:** arming time + W, rounded up to the second.

**Predicates.** All must hold:
1. **Trigger past threshold:**
   - concentrated liquidity: the price word, mask 2^160−1, `>=` or `<=`
     the threshold sqrtPriceX96;
   - Aerodrome v2: one reserve `>=` and the other `<=` their threshold
     values, so a mint cannot fire it.
2. **The other pool in a band:**
   - concentrated liquidity: its masked price word `>= lo` and `<= hi`;
   - Aerodrome v2: both reserves within a band.

   The band is the widest symmetric one, capped at ±5 bps of price, in
   which the plan at `s*`, with the trigger exactly at threshold, still
   clears `H` at both corners.
3. **The other pool's liquidity:** `>= 0.98 ×` its current value
   (concentrated liquidity). The trigger pool gets no liquidity predicate:
   its liquidity changes legitimately as it moves, and the floors guard it.
4. **`block_number <= armed block + ceil(W / block_time)`.** W = 10 s by
   default (`block_time` 2 s today). No `flashblock_index` predicate.

**Senders and windows.**
- An arm holds its sender until its window passes. Senders are assigned
  least-recently-freed first, so a slot's next arm normally lands on a
  different sender, with no gap.
- A slot is re-armed when:
  - its window expires;
  - the other pool leaves its band;
  - base fee or L1 cost moves the hurdle by more than 10%;
  - its rank falls out of the top N.
- A superseded arm cannot misfire: its band or threshold predicate is false,
  or it fires on a state that is still profitable.
- If two arms on one slot become eligible together, the first to execute
  closes the gap; the other's predicates turn false and it is never
  included.

**Bid.** The priority fee per gas is `min(cap, f × (π_r − base costs) / g)`.
`π_r` is the expected gross at the rung's threshold, `g` the gas estimate,
and the base costs are base gas and the L1 fee. With `margin_wei: 0`, the
hurdle is "surplus over base costs > 0". Any `f < 1` keeps that surplus
positive after the bid, so a rung's threshold is the same for every `f`.
In armed-shadow no `f` is bound: the ledger records `π_r`, `g` and the
costs, and the evaluator sweeps `f` exactly. In armed-live `f` comes from
gate A.

**The armed ledger,** `var/apex/armed-YYYY-MM-DD.jsonl`, one line per arm:
- `arm_id`, `armed_at_ns`, `armed_block`, `to_block`;
- slot: `route_hash`, `trigger_pool`, `other_pool`, `side`, `rung`;
- threshold: price word or reserves, and the move in bps;
- the predicates, exactly as sent in armed-live;
- plan: `size_wei`, the per-hop `minOut`, `minProfit`, `gas_limit`,
  `deadline`, the calldata and its keccak;
- expected: `gross_wei` at threshold, `gas_units`, `l1_fee_wei`,
  `base_fee_wei`;
- `sender_slot`, and in armed-live the signed transaction's hash and the
  sequencer's answer.

Writes go through R25's `ledger::append_line`, off the capture path.

### 3. The evaluator (code, `apex armed-eval`, every 5 minutes on the host)

A systemd timer runs it on arms whose window closed at least 60 s ago.

1. **Logs:** one `eth_getLogs` per run over every armed pool (Swap, Sync,
   Mint, Burn) for the blocks the closed windows cover.
2. **Firing:** the predicate words are rebuilt after every transaction:
   - Swap events carry the post-swap sqrtPriceX96, tick and liquidity;
   - Sync events carry the reserves;
   - a Mint or Burn in range makes the liquidity word uncertain until the
     next Swap, and is flagged.

   The fire point is the first transaction after which every predicate
   holds.
3. **Profit:** the step-exact quoter runs on the two pools' state. That
   state is `PoolBook::load` at the fire block's parent, plus that block's
   Swaps and Syncs up to the trigger, replayed with the existing writes. The
   calldata's size and floors are quoted there, and gas, L1 fee and the
   base fee at the fire block are subtracted.
4. **Check:** while fires are few, all of them; later 1 in 10.
   `eth_simulateV1` runs on the parent block: the fire block's earlier
   transactions that touched either pool are replayed from their
   transaction objects, then the armed call is sent from the authorized
   trader `0xCB43…`. Quote and simulation must agree to the wei on gross,
   and gas used must stay within the limit.
5. **The rival:** the first transaction after the fire point that makes our
   predicates false again, whatever it is: an arbitrage, or any swap that
   moves the price back. Its flashblock comes from the flashblock ledger,
   its priority fee from its transaction.
6. **Win or loss,** for each `f` in {0.05, 0.1, 0.2, 0.3, 0.5, 0.7, 0.9}:
   - a rival in a later flashblock than the fire point: a win;
   - a rival in the fire flashblock: a win if our bid exceeds its priority
     fee;
   - no rival in the window: a win.

   Two landing variants are reported: optimistic (we land in the fire
   flashblock) and pessimistic (the next, where a rival in that next
   flashblock competes by fee).

**Output.**
- `var/apex/armed-verdicts-YYYY-MM-DD.jsonl`, per arm:
  - `fired`, and the fire point (block, transaction index, flashblock);
  - the quoted gross; the simulation's gross, gas and success;
  - the rival (block, index, flashblock, transaction, priority fee) and its
    offset;
  - per `f` and per variant, win and net.
- A daily summary per `f`: fires, wins, net in USD (WETH priced from the
  book), and the fee cost of fires whose quote fell below break-even. These
  are the included reverts.

The evaluator's BlockPI load is one `eth_getLogs` per run, plus per fire a
two-pool book load, one transaction read and one simulation. It counts
against BlockPI's prepaid request units, which ran out once already, on
2026-10-12 at 09:40 UTC.

### 4. The live pilot (code and operator steps)

**Senders.**
- N new keys, made on the host by `apex keys new --count N`, written to
  `/etc/apex/armed.keys`, owned by `apex` with mode 0400. Only addresses
  are printed. Boot refuses the file if it is group- or world-readable.
- They are separate from the shadow's trader `0xCB43…`, so the R25
  nonce tripwire stays valid.
- **Authorization:** I prepare and dry-run one owner transaction,
  `BatchRouter.multicall([clone…], [setExecutor(kᵢ, true)…])`, and its
  revocation (`false`). The operator broadcasts it.
- **Funding:** the operator sends each sender 0.002 ETH. A sender below
  0.0005 ETH is left out of assignment and reported.

**Submission.** `base_sendRawTransactionValidity` to
`https://mainnet-sequencer.base.org`, from the host, never through BlockPI.
Every answer is recorded in the armed ledger: a hash, or the full error. An
"underpriced replacement" on a reused nonce bumps both fees by 12.5%, at
most twice, then frees the slot.

**Caps.** All in config, all enforced before signing:
- `daily_loss_cap_usd: 25`: net of the UTC day's realized profits. When
  hit, the pilot disarms until 00:00 UTC; outstanding arms expire within W;
- `max_priority_gwei`, `max_bid_fraction` (= gate A's `f`),
  `max_size_weth`, `max_arms` (= N);
- a kill file, `/opt/apex/var/armed.disable`: present means disarmed.

**Accounting.** Every landing's receipt gives `gasUsed ×
effectiveGasPrice` plus the L1 fee, and `ProfitRealised` (gross and owner
fee; both are ours). They go to `var/apex/armed-landings-YYYY-MM-DD.jsonl`
and into the report's `armed` section: arms, landings, gross, fees, net.

**Tripwires.**
- A sender's nonce that moves without a recorded landing: disarm everything
  and log ERROR every head.
- A landing whose sender is not one of ours: impossible by construction;
  checked anyway.

### 5. Configuration (`ops/shadow.base.yaml`, an `armed:` section)

```yaml
armed:
  mode: shadow          # shadow | live; live also needs gate A's approval
  ledger: var/apex/armed.jsonl
  verdicts: var/apex/armed-verdicts.jsonl
  max_arms: 16          # virtual senders in shadow; real senders in live
  rungs: 3              # moves m1, 2·m1, 4·m1 per slot
  window_s: 10
  block_time_ms: 2000   # 200 after Denim
  margin_wei: 0         # hurdle = surplus over base gas and L1 fee > margin
  band_cap_bps: 5
  liquidity_floor_bps: 9800
  live:
    keys_file: /etc/apex/armed.keys
    sequencer: https://mainnet-sequencer.base.org
    daily_loss_cap_usd: 25
    max_priority_gwei: null   # set at gate A
    max_bid_fraction: null    # set at gate A: the bid curve's best f
    max_size_weth: null       # set at gate A
    kill_file: var/apex/armed.disable
```

`mode: live` refuses to boot while any `live` value is null, so gate A has
to set them.

### 6. Gates

- **Gate A, armed-shadow to the pilot:**
  - at least 3 days of armed-shadow and at least 30 fires;
  - the pessimistic bid curve has a positive net per day at some `f`;
  - quote and simulation agree on at least 95% of checked fires;
  - the operator approves, then broadcasts the authorization and funds the
    senders.
- **Gate B, the pilot to scale:**
  - 7 days live with realized net positive after every fee;
  - the fee cost of included reverts below 20% of gross;
  - zero tripwire events.

  Then more senders and higher caps.

## What does not change

- The reactive shadow path, its plane, its Tier 2 and the trader
  `0xCB43…`.
- The executor contract.
- R25's race and flashblock ledgers, which keep running. The race
  analyzer's `rival_offsets` decides how much armed-live is worth: offset 0
  is exactly the rival this targets.

## Denim

Base's Denim upgrade (planned: Sepolia October 2026, mainnet November 2026;
not yet scheduled) replaces flashblocks with 200 ms blocks. R26 is built to
survive it:

- windows are in seconds; `block_number` bounds come from `block_time_ms`;
- there are no `flashblock_index` predicates;
- the evaluator's "same flashblock" becomes "same block";
- predicates and fees are unchanged.

The feed's own move off `pendingLogs` is R27's.

## Verification

- **`move_to_price`:** for recorded Swap events that crossed ticks (Uniswap
  v3, PancakeSwap, Slipstream), moving the pre-swap state to the event's
  post-swap price reproduces its tick and liquidity exactly. For Aerodrome
  v2, the reserves match the following Sync within rounding against us.
- **The threshold search:** a property test that profit at a fixed size is
  non-decreasing in the move, and that the found move is minimal (one step
  less fails `H`).
- **Predicate encoding:** golden JSON for each venue, byte for byte against
  the RPC's schema.
- **Slots:**
  - at boot and after every full reload, the arming engine reads each
    pool's price word, liquidity word and reserve words with
    `eth_getStorageAt`, at the block the book was loaded at, and compares
    them with the book's state. A mismatch leaves the pool unarmable and is
    reported;
  - `scripts/data/verify_pool_slots.py` does the same offline for the
    whole inventory, and a test pins the venue table on recorded words.
- **The evaluator:** a recorded real case (a block with a trigger and a
  rival: logs, receipts, flashblock lines) gives the expected verdict for
  every `f`.
- **The pilot's API probe** (the pilot's first hour, ~US$0.02). It uses one
  sender:
  1. a zero-value self-transfer whose storage predicate is false must not
     be included and must cost nothing;
  2. the same with a true predicate must be included next block;
  3. an expired arm's nonce reused without a bump shows how Base treats
     expiry;
  4. a transaction carrying the largest predicate set the engine produces
     (8: two trigger, four band, one liquidity, one block) must be
     accepted.
- **Mutations,** as usual: the threshold side, the band corners,
  break-even floors, expiry arithmetic, the rival rule's flashblock
  comparison, and the loss cap.

## Risks

- **Rivals arm too.** Then wins go to the higher bid, and margins compress
  toward the cost of the trade. The bid curve measures this; gate A will not
  pass a curve that is negative.
- **Builder ordering inside a flashblock is approximated** in the evaluator
  (fees at selection time, Base's dynamic mempool). That is why the two
  landing variants exist; the pilot's receipts are the ground truth.
- **Undocumented limits:** predicate count, request rate, and how an
  expired transaction's nonce is treated. The API probe settles all three
  before any armed trade.
- **Included reverts cost fees.** Break-even floors make them rare, and
  their cost is reported per day and capped by the loss limit.
- **Key exposure:** senders hold about 0.002 ETH each, the executor holds no
  capital, and the revocation is prepared before funding.
- **RPC budget:** the evaluator and the arming engine add BlockPI load; the
  report counts the calls. A second provider is a separate change.
- **Denim:** if it activates before R27, the feed goes blind
  (`pendingLogs` ends); the arming engine and the evaluator do not depend
  on flashblocks for anything but offsets.

## Rollout

1. Build, test, gates, mutations; commit; verify in a clean worktree.
2. Deploy to the host as a new revision in `mode: shadow`. This restarts
   R25's 14-day clock; the race ledger continues, and by then R25 will have
   at least three days of contested gaps.
3. Three days of armed-shadow; read the bid curve with the race analyzer's
   offsets.
4. Gate A: the operator approves. I generate the senders on the host and
   prepare and dry-run the authorization; the operator broadcasts it and
   funds the senders.
5. The API probe, then `mode: live` under the caps.
6. Gate B after 7 days.
