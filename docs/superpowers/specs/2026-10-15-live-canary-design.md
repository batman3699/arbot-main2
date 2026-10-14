# R28: the live canary, and a priority fee on the reactive lane

Task 8.5 R28. The operator asked for this on 2026-10-15 ("proceed with Live
canary, plus priority-fee bidding") and approved the design in conversation
the same day, with four decisions: a loss cap, a 20% tip, profit to the
owner's address, and the deadline left at the block being built. The trader
starts with what the owner holds, 0.0014 ETH, so the limits below are scaled
to it.

## Goal

The reactive lane takes real trades, one at a time, with every loss bounded
and every trade reconciled to the wei. It bids a priority fee of a fifth of
each trade's expected net. The armed lane stays in shadow.

## Evidence

**The plane already runs the whole live protocol.** `Plane::protocol` signs,
dispatches to `DispatchLane::Live`, asks the settlement feed for the
transaction's stages, and reconciles through
`ChainExecutionAdapter::reconcile_final_state`. The lifecycle tests drive it
with no network. What a sending run lacks is four network pieces, each a stub
that refuses today:

- **A sender.** Only `NullDispatcher` implements `Dispatcher`.
- **A settlement feed.** The shadow's `NothingSent` refuses every question.
- **Reconciliation.** `LiveAdapter` holds a `RefusingRpc`, so
  `reconcile_final_state` refuses. `apex_chain::base::reconcile::reconcile`
  computes a `PnlAttribution` from a receipt, but nothing feeds it one.
- **Recovery for a journal that sent.** A shadow journal is closed as never
  sent. The journal records no transaction hash or nonce
  (`OpportunityTicket::nonce` is `None` on every ticket the plane builds), so a
  crash after sending leaves nothing to look up.

**Nothing bounds a loss.** `LiveRiskGate` holds a `CircuitBreaker` with every
limit at its maximum, and nothing records an outcome into it.

**Signer funding is checked nowhere** (PLAN.md, R9 finding).
`LaneRequirements::min_gas_reserve_wei` and `LastMileContext::
required_reserve_wei` are both 0.

**On chain, read 2026-10-15 through `mainnet.base.org`:**

- The trader `0xCB43…6595` holds 0 ETH, nonce 0, and `executors(trader)` is
  true.
- The owner `0x69D5…8A3` holds 0.001465 ETH.
- The executor pays profit to `profitRecipient` and a 3% fee
  (`getConfig().feeBps = 300`) to `feeRecipient`. **Both are the BatchRouter**
  `0x9ecd…52C2`, which has no withdraw function. The owner could still recover
  the profit through `setTargetAllowed(WETH)` and a `multicall` transfer.
- `isCircuitOpen()` is false.

**The on-chain floor is too high, not too low.** PLAN.md's R6 finding proposed
raising it to cover the execution cost. The arithmetic says otherwise:

- `Plane::lock` sets `min_profit = max(expected_net_profit, 1)`.
- Hop 2's minimum is `max(loan + min_profit, slippage bound)`, and the executor
  also checks the WETH surplus against `min_profit` after both swaps.
- On Base a reverted transaction is included and pays for its gas. A revert at
  hop 2 or at the final check has run both swaps, so it costs nearly a full
  trade's gas.
- Once a trade is sent, landing it with any positive gross loses less than
  reverting it. Landing nets `G − C`; reverting nets about `−C`.
- Today a trade whose gross falls between its cost and its expected net
  reverts. Expected gross 10 and cost 4 give a floor of 6. A realized gross of
  5 would have landed at +1, and reverts at −4 instead.

The protection belongs where the decision to send is made, and it is already
there: the expected net, after every cost, must clear the policy's minimum.

**An expired plan reverts early.** `_initiateLoanV2` checks the deadline
before the loan is taken, so a late landing costs the intrinsic gas and the
calldata, about 60,000 gas.

**Rivals' priority fees** (R25 contest analysis, run-5, 60 contested gaps): a
median of 0.004 gwei, 0.40 at the 90th percentile, at most 0.98, and zero on
9. Of our 40 signed tickets, 4 would have landed in the same flashblock as a
rival while bidding zero.

## Design

### 1. `apex canary`: a command that sends

`apex canary --config ops/canary.base.yaml` runs the shadow's process with one
difference: the reactive plane's lane is `DispatchLane::Live`. The armed lane,
the reports, the race ledger and the flashblock ledger are unchanged.

- **`apex shadow` stays unable to send.** Its lane still holds an
  `Arc<NullDispatcher>`, a type that cannot send. The loader refuses a
  `canary:` section in a shadow's configuration.
- **The canary refuses a configuration without one**, and a journal not named
  as a canary's (`var/apex/canary.journal`). Each command's recovery is true
  only of its own journals.
- **A systemd unit, `apex-canary.service`,** has `Conflicts=apex-shadow.service`.
  The two share the trader key and must never run together.

Starting the canary ends the current shadow run (archived as `run-10`).

### 2. The configuration

`ops/canary.base.yaml` is `ops/shadow.base.yaml` with these differences:

```yaml
journal: var/apex/canary.journal

signer:
  # 0.0001 ETH kept back: the trader starts with 0.0014.
  gas_reserve_wei: 100000000000000

policy:
  # A live run keeps the default (operator, 2026-10-06).
  max_cost_confidence_bps: 1000
  # In both files, so shadow and canary price the same trade the same way.
  tip_share_bps: 2000

canary:
  ledger: var/apex/canary.jsonl
  stop_file: var/apex/canary.stop
  # The executor must pay here, or the canary refuses to start.
  profit_recipient: "0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3"
  # Losses on losing trades; wins do not offset them.
  loss_cap_wei: 1000000000000000
  max_consecutive_reverts: 5
  max_consecutive_refusals: 2
  max_landed: 100
  receipt_poll_ms: 250
  settle_timeout_ms: 10000
```

`policy.tip_share_bps` is added to `ops/shadow.base.yaml` as well, so the
shadow prices the same trade the same way.

### 3. The on-chain floor is one wei

`Plane::lock` commits `min_profit = 1`. Hop 2 must then return the loan and
one wei. The executor's final check passes any positive surplus.

- **The decision to send is unchanged.** The risk gate's eligibility clauses
  and last-mile check 6 still require the expected net, after every cost, to
  clear `policy.min_profit_wei`.
- **The commitment covers the new floor**, as it covered the old one.

### 4. A priority fee of a fifth of the expected net

**In the economics** (`LiveEconomics::assemble`):

- `net₀` is the expected net with no tip, as today.
- `tip = tip_share_bps × max(net₀, 0) / 10,000`, charged as
  `TotalExecutionCost::priority_fee`.
- The expected net becomes `net₀ − tip`.

**The size does not change.** The tip is proportional to the net, so the size
that maximizes `net₀` also maximizes `net₀ − tip`. The tip is applied after
sizing, and the certified search is untouched.

**One approximation, stated:** a revert pays the tip on the gas it used, and
the expected failure cost does not include that. The tip is a fifth of the net
and the failure prior is 5%, so the error is under one percent of the net.

**In the signer:**

- `fee_caps` takes the candidate.
- `max_priority_fee_per_gas = priority_fee / gas_used_distribution.p50`,
  rounded down.
- `max_fee_per_gas = min(2 × observed base fee + tip per gas, fee ceiling)`.

Last-mile revalidation already refuses a base fee above the ceiling.

The armed lane keeps its own bid.

### 5. The signer must hold what a trade can cost

On the live lane only:

- `LaneRequirements::min_gas_reserve_wei = signer.gas_reserve_wei`.
- Last-mile check 7's `required_reserve_wei` = gas limit × max fee per gas +
  `signer.gas_reserve_wei`.

The shadow's trader holds nothing, and a shadow ticket that failed this check
would measure the wallet, not the trade. So the shadow lane keeps zero, and
says so.

### 6. The sender

`CanaryDispatcher` implements `Dispatcher`. For each request:

1. **The canary's limits are checked.** A halted canary refuses the request,
   though the risk gate (§8) should already have stopped it.
2. **A send record is appended to `canary.jsonl` and fsynced**, before
   anything leaves the process: ticket, nonce, transaction hash, gas limit, fee
   caps, predicted gross and net, and the time.
3. **`eth_sendRawTransaction`** goes to the first `rpc.http` endpoint, BlockPI's
   keyed and MEV-protected one, with the request's exact signed bytes (INV-10).
4. **The answer is returned:**
   - success → a ladder at `TransportAccepted`;
   - an error response → `DispatchError::Rejected` with its message;
   - no answer → `NoResponse`.

**A rejection releases its nonce, unused.** The endpoint said the transaction
went nowhere. Today the plane returns on a dispatch error before releasing
anything, so the next ticket would take the nonce after it and wait behind a
gap forever. A rejection costs nothing and counts toward
`max_consecutive_refusals`.

**No answer halts the canary as unresolved** (§8). The transaction may or may
not have left, and only the chain can say which.

The send record is written before the send, so a crash can never leave a sent
transaction the canary does not know about. It can leave a send record for a
transaction that never left; recovery finds no receipt for it and the nonce
unconsumed.

### 7. Settlement and reconciliation

**`CanarySettlement` implements `SettlementFeed`.**

- `observe(tx)` polls `eth_getTransactionReceipt` every `receipt_poll_ms`
  until there is a receipt or `settle_timeout_ms` has passed.
- A receipt is `Included`. Finalization is an L1 fact the canary does not
  wait for.
- One ticket is in flight at a time: the capture loop is serial, and it waits
  for this. A sent trade lands within a block or two, so the loop pauses for
  about 1–2 s. At a few trades a day this costs nothing measurable.
- **Timeout:** the canary reads the trader's latest nonce.
  - The nonce was consumed → it looks for the receipt once more.
  - Otherwise, or if it still cannot find one → the canary **halts as
    unresolved**.
  - In both cases the plane closes the ticket as not included and releases the
    nonce, as it does today. A halted canary signs nothing more, so a released
    nonce is never reused while the transaction may still land.

**Reconciliation is a real `BaseRpc::attribute`** behind a `BaseAdapter`. That
adapter replaces the shadow's `LiveAdapter` in the canary, and refuses
everything else, as `RefusingRpc` does.

- `attribute(tx)` takes the receipt the settlement feed found, and the
  ticket's context that the sender recorded under the hash: ticket, route
  hash, gas limit, venues.
- **The gross** is the WETH the executor transferred to `profitRecipient` and
  `feeRecipient`, read from the receipt's `Transfer` logs. It must equal
  `ProfitRealised.profitDelta` from the same receipt, or the trade is
  **unexplained** and the canary halts.
- **The cost** is the receipt's `gasUsed × effectiveGasPrice` plus its `l1Fee`.
- `reconcile` returns the `PnlAttribution`. Its net is `gross − cost`.

**A revert closes the ticket as a revert, not as an unavailable chain.**

- A new `Decline::SettlementReverted { gas_used, spent_wei }` maps to
  `TerminalFailure::Reverted`.
- The nonce is released as **landed**: a reverted transaction consumed it.
- The revert's reason is asked for once, by `debug_traceTransaction` with the
  call tracer. Where the endpoint will not answer, the revert is classed by
  where it stopped: under 100,000 gas is **early** (the deadline, the
  commitment, the caller), and above is **late** (a hop's minimum or the
  final check).

**Every outcome is appended to `canary.jsonl` and fsynced:** landed or
reverted, block, gas used, effective gas price, L1 fee, gross, cost, net,
predicted gross, and the revert class.

### 8. Limits and kill switches

`Canary` holds the limits and the running totals. It rebuilds them from
`canary.jsonl` at boot.

**It halts on any of these, and stays halted:**

| Halt | When |
|---|---|
| Loss cap | the sum of losing trades' net losses reaches `loss_cap_wei`; a revert's loss is its whole cost |
| Reverts | `max_consecutive_reverts` reverts in a row |
| Refusals | `max_consecutive_refusals` refused sends in a row |
| Unresolved | a sent transaction with no receipt at its timeout (§7), or a send with no answer (§6) |
| Unexplained | a landed trade whose transfers and `ProfitRealised` disagree |
| Tripwire | the trader's pending nonce passes the boot nonce plus our own sends |
| Done | `max_landed` trades have landed |

- **A halt is a ledger record**, so it survives a restart. The operator clears
  it with `scripts/canary-reset.sh`, with the service stopped. The script
  appends a reset record, and the boot replay counts the cap from it.
- **A halted canary keeps running as a shadow does.** The risk gate refuses
  every candidate with the halt's reason (`RiskRefused`), so the funnel, the
  race ledger and the near-misses keep measuring.

**The kill switches:**

- **Soft:** `touch var/apex/canary.stop`. The risk gate refuses while the file
  exists, and sends resume when it is removed. It is not a halt.
- **Hard, on chain, the owner's own:** `tripCircuit()` or
  `setExecutor(trader, false)`, through the router's `multicall`.
  - The head task reads `executors(trader)` and `isCircuitOpen()` every block,
    beside the executor's code.
  - Either one blocking makes the executor's version read as 0, which
    last-mile check 2 refuses. So a flipped switch stops sends before any gas
    is spent on them.
  - Two `eth_call`s a block add about 1% to the host's RU.
- **Off:** `systemctl stop apex-canary`.

**The tripwire changes meaning.** The shadow's tripwire ("the signer's nonce
moved") becomes, in the canary, "the nonce moved beyond our own sends". It
still means the key is in use somewhere else. It still logs at ERROR and sets
`signer.nonce_moved`, and it now halts as well.

### 9. Recovery

`Plane::boot` takes a `ChainOutcomeSource`. The canary's is built before boot,
from the ledger and the chain:

| The ticket, open in the journal | Its outcome |
|---|---|
| No send record | never sent: `Abandoned`, "no send record: never left the process" |
| A send record and a receipt | reconciled from the receipt, as in §7, and recorded |
| A send record, no receipt, the nonce unconsumed | `Abandoned`, "sent, unresolved at boot"; the canary halts as unresolved |

The dispatch gate then opens, as INV-39 requires. An unresolved transaction
leaves the canary halted rather than the process down, so systemd does not
loop and the run keeps measuring.

### 10. What the operator does first

I prepare and dry-run each of these; the owner broadcasts, with the key typed
at the prompt.

1. **Point profit at the owner.** `router.multicall([clone, clone],
   [setProfitRecipient(owner), setFeeRecipient(owner)])`.
   - The router is a config admin (`configAdmins(router)` read true).
   - Dry runs: as the owner it succeeds; as the trader it reverts `NotOwner`;
     `cast estimate` gives the gas.
   - The canary refuses to start until `profitRecipient()` reads the
     configured address.
2. **Fund the trader: 0.0014 ETH** from the owner. That leaves the owner
   ~0.000065 ETH for the kill switch, more than 100 owner transactions at
   today's fees.

The profit arrives at the owner as WETH. Unwrapping it calls WETH, which
BlockPI's endpoint refuses (PLAN.md, the R27 dust seeding); use
`https://mainnet.base.org` for that.

### 11. Reports

- **The five-minute report gains a `canary` block:**
  - counts: sends, landed, reverted (early and late) and refused;
  - money: realized gross, cost and net, predicted against realized gross on
    each landed trade, and the losses counted toward the cap;
  - state: the halt and its reason, the stop file, and the trader's balance.
- `scripts/shadow-status.sh` prints a `canary` line when the block is present.
- `ops/host/README.md` gains the canary's install, checks, reset and kill
  switches.

## Testing

Every guard gets a test that fails without it, and a mutation run.

- **Floor.** `lock` commits `min_profit = 1`. A plan built from it sets hop 2's
  minimum at the loan plus one wei.
- **Tip.**
  - `assemble` charges a fifth of `net₀`, and the expected net is the rest.
  - A negative `net₀` pays no tip.
  - The size is the same with and without the tip.
  - `fee_caps` signs the tip per gas, rounded down, and `2 × base + tip` under
    the ceiling.
- **Funding.** A live-lane ticket is refused when the signer holds less than
  its gas limit × max fee plus the reserve. A shadow-lane ticket is not.
- **Ledger and limits.**
  - Each halt in §8 trips at its threshold and not one short.
  - Wins do not offset the loss cap.
  - A halt survives a restart, and a reset clears it.
  - The stop file refuses while present and not after.
- **Sender.**
  - The send record is on disk before the request leaves. A transport that
    asserts the file exists when called proves it.
  - Rejections and silences map to their `DispatchError`s.
  - A rejection releases the nonce unused: the next ticket signs at the same
    nonce. A silence halts as unresolved.
- **Settlement and reconciliation**, from receipts recorded on Base:
  - the gross from the transfers to the recipients;
  - the cost from `gasUsed`, the effective price and `l1Fee`;
  - transfers and `ProfitRealised` disagreeing → unexplained;
  - a revert → `SettlementReverted`, with its nonce released as landed;
  - a timeout with the nonce unconsumed → unresolved.
- **Recovery.** Each row of §9's table.
- **Kill switches on chain.** A view with `executors(trader)` false, or the
  circuit open, reads the executor's version as 0.
- **End to end on a fork** (ignored by default; network). Anvil forks Base at
  its latest block from `https://mainnet.base.org` (not the shadow's BlockPI
  key, which a fork's burst of reads would rate-limit). The proof deploys
  nothing and uses no real key. On the fork it impersonates the owner
  (`anvil_impersonateAccount`) to authorize an anvil dev key through the
  router (`setExecutor(dev, true)`), and that key signs.
  - A whale's swap opens a gap, as R6's proof did.
  - A plane on a live lane, with `CanaryDispatcher`, `CanarySettlement` and the
    reconciling adapter all pointed at anvil, takes the trade.
  - The ticket closes `Reconciled`, the realized gross equals the prediction
    to the wei, and the ledger holds both records.
  - A second trade on a stale plan reverts and is counted. A third, with the
    stop file present, is never sent.
- **The configurations.** `ops/canary.base.yaml` loads; the shadow's refuses a
  canary section, and the canary's refuses a shadow journal.
- All five CI gates, and the commit verified in a clean worktree.

## Rollout

1. Build on `task-8.5-r28-canary` under every gate, verify the commit in a
   clean worktree, and merge.
2. The owner's two transactions (§10).
3. On the host:
   - stop `apex-shadow` and archive `var/apex` as `run-10`;
   - install the build and `apex-canary.service`;
   - start the canary.
4. **The first send is also the test of BlockPI's address screen** for a
   trader → executor transaction (PLAN.md, the R27 dust seeding). A refusal
   costs nothing. Two in a row halt the canary with the endpoint's message,
   and the choice of another endpoint is the operator's.

**Success:**

- 100 landed trades or 14 days;
- a positive aggregate realized net after every cost;
- zero unexplained losses;
- on uncontested trades, a realized gross equal to the prediction to the wei.

**Stop and review** on any halt but `Done`.

## Risks

- **Real money moves.** It is bounded by the trader's balance (0.0014 ETH), the
  loss cap (0.001 ETH) and the halts. The flash loan is repaid inside the same
  transaction or the transaction reverts, so the size puts nothing but gas at
  risk.
- **BlockPI's screen may refuse a trader → executor transaction.** It costs
  nothing, and the first send says so.
- **A one-wei floor lands trades that net less than expected, or lose a
  little.** That is the point (§3). Each one is recorded against its
  prediction, and the loss cap bounds the total.
- **The serial loop waits for settlement.** A burst of candidates during a
  trade's 1–2 s settlement is handled late or declined stale. The race ledger
  records it.
- **Base Denim (planned for November)** ends flashblocks and the `pending`
  tag. The canary's receipts work either way; the feed it depends on does
  not.

## Not in R28

- **Sending on more than one endpoint, or to the sequencer directly.**
  Measure the first lane first.
- **The armed lane's live sends** (validity transactions).
- **A next-block deadline.** The operator kept the current block. The canary
  counts how often a late landing reverts early.
- **More than one ticket in flight, or more than one trader.**
- **Refunding or topping up the trader automatically.**
