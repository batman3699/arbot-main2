# R29 — the lean executor (design)

Task 8.5, R29. Asked for on 2026-10-15 as the second of four (BlockPI traffic,
a leaner executor, more pools and tokens, a measured tip); started 2026-10-17
("proceed with leaner executor").

## Why

A priority fee is bid per unit of gas. The canary signs a tip worth a share of
the expected net (`policy.tip_share_bps`, 20%), so for the same trade, a path
that burns half the gas bids twice as much per gas. R26's first armed fire lost
exactly this way: a rival paid 1.81 Mwei/gas on 238k gas, where our 560k-gas
path capped the bid at 0.88 Mwei/gas.

The canary's five live sends so far have all reverted. Each was checked against
the chain: the book was exact every time, and each revert was a race lost or a
block missed. Gas is not the whole of the race, but it is the part of the bid
this repository controls.

## Where the gas goes (measured)

Ticket 664's plan (WETH → cbBTC on Slipstream V3 tick spacing 1, cbBTC → WETH
on Uniswap 0.3%, 1.9375 WETH from Balancer), simulated with `cast call --trace`
at block 52,671,989, where it succeeds and returns the expected gross to the wei:
**566,972 gas used**.

| Part | Gas | |
|---|---|---|
| The two pools' swaps | ~300k | Slipstream V3 234k (its fee module's `observe` 57k, gauge fee checks 21k, cbBTC's proxy transfer 44k); Uniswap 0.3% 67k |
| The executor's own machinery | ~150k | `PlanV2` ABI-encoded into the loan's context, decoded again in the callback, hashed for the commitment; start balance and gross profit written to storage; config, role and circuit reads; an external self-call through the proxy for the steps |
| Routers and approvals | ~70k | every hop goes through the venue's router; an exact approval set each trade (22k) and a max approval on first use (26k) |
| Balancer flash loan | ~38k | |
| Profit to the owner | ~28k | two transfers (fee and remainder, both to the owner) and `ProfitRealised` |
| First holding of cbBTC | ~17k | the executor's cbBTC balance slot goes from zero |

The pools are the venues' cost and stay. Everything else is ours.

## Goals

- **About half the gas.** Ticket 664's route at the same block on a fork:
  **≤ 340,000 gas used** (−40%), with the venues' 300k unchanged. A target,
  not a gate: a build that lands above it is reported with the measured
  breakdown, not abandoned. A route on
  Uniswap and PancakeSwap pools only, whose swaps cost ~70k each, should come in
  near half of today's.
- **The same trades, exactly.** Realised gross equals the prediction to the wei,
  shown on a fork through the canary's own sender, settlement and
  reconciliation, as R28's fork proof was.
- **The canary switches to it straight away** (operator, 2026-10-17): no
  side-by-side run.

## Non-goals

- No change to search, pricing, sizing or the risk gate.
- No new venues (Uniswap v4 and the rest stay a later lever).
- No inventory trading: trades still borrow, so the executor holds nothing
  between trades.
- No upgradeability. A new address is cheaper than a proxy on every call.
- The armed lane stays in shadow.

## Anti-goals

It fails, however low the gas, if:

- anyone but the authorized trader can make it move tokens, or a call into one
  of its callbacks from outside a trade can pull anything;
- a trade can complete without repaying the loan and clearing the profit floor;
- the owner loses either on-chain stop: revoking the trader, or the circuit
  breaker;
- the Rust encoder and the contract can disagree about what a plan says without
  a test failing.

## Decided by the operator (2026-10-17)

- **On-chain checks dropped:** the plan commitment hash, the adapter allowlist
  (registry and selector allowlist), the declared-residue/multi-asset balance
  invariant, the canonical profit token, and the slippage config guard.
  Offered the list and asked whether to keep any: "no".
- **Kept:** trader-only access, the plan deadline, the profit floor (the loan
  repaid and at least `minProfit`, 1 wei, left), and the owner's circuit
  breaker.
- **Profit goes to the owner on every trade.** A sweep instead only if that
  transfer were ever the difference between landing and not; with the owner
  holding a few wei of WETH it is ~10k gas, about 3% of a trade, so it is not.
- **Straight to the canary**, no side-by-side run.

## Designer's calls (each the builder may revisit with a measurement)

- **A new, non-upgradeable contract.** The owner, the trader set and the
  recipients as today (`setExecutor`, `setProfitRecipient`, owner-only
  `sweep`); WETH, the lenders and the venues' factories as immutables or
  constants, not storage.
- **Pools called directly, paid in their own callbacks.** No routers, no
  approvals. `uniswapV3SwapCallback` for Uniswap v3 and both Slipstream
  factories (Slipstream's pools call it under that name; the trace shows it),
  `pancakeV3SwapCallback` for PancakeSwap v3. Aerodrome v2: transfer in, then
  `swap` for the exact out amount, with empty data so no hook runs.
- **A callback pays only the pool the contract is swapping with right now.**
  The expected pool is set immediately before each `swap` and checked and
  cleared in the callback; outside a trade nothing is expected, and every
  callback reverts. Transient storage (EIP-1153, live on Base) makes this ~200
  gas; it needs Solidity ≥ 0.8.24 for this contract.
- **A packed plan, read from calldata**: the loan, the deadline, `minProfit`,
  and per hop the pool, direction, amount and minimum out. Nothing about the
  plan is written to storage, and nothing is re-encoded for the callback that
  is not needed there.
- **Each hop spends what the one before it returned**, not a planned amount
  carried in the plan, so no intermediate balance is left behind for the
  dropped residue check to have caught. The first hop spends the loan. The
  search builds two-hop cycles (`frontier::build`); the plan takes any number
  of hops, and the gas target is set on two.
- **No price limit on a swap** (`MIN_SQRT_RATIO + 1` / `MAX_SQRT_RATIO - 1`, as
  the routers pass for none): the hop minimum is the bound.
- **Hop minimums stay.** Each hop's output is checked against its minimum (the
  exact quote), as today. It costs a comparison, and R28's INV-17 canary waiver
  names "hop minimums at the exact quote" as one of the stand-ins for the
  certificate.
- **The same view surface the runtime reads.** `executors(address)` and
  `isCircuitOpen()` with today's signatures, which the live reader calls every
  block (last-mile check 2), and `profitRecipient()`, which the canary checks at
  boot. There is no fee recipient, so that boot check reads the profit
  recipient alone. The reader also holds the executor's code hash to the
  configured `executor.code_hash`, which the switch updates.
- **One profit transfer and one small event.** A single WETH transfer of the
  gross profit to the profit recipient, and one event carrying the gross, which
  `canary::chain` reconciles against as it does `ProfitRealised` today (new
  signature, same rule).
- **Dust.** A few wei of each intermediate token the universe trades through,
  held by the new executor, and a few wei of WETH at the owner address, so no
  trade pays to create a balance slot. The owner holds none of those tokens, so
  seeding is one owner transaction, prepared and dry-run here, that buys a few
  wei of each and sends them to the executor, as R27's WETH dust was seeded.
- **The lender.** Balancer V2's `flashLoan` against Morpho Blue's (`0xBBBB…EFFCb`,
  no fee, ~82k WETH) called directly rather than through R27's ERC-3156 wrapper:
  measured on a fork, the cheaper is the default and the other covers sizes
  beyond the default's holding. R27's `MORPHO_EXTRA_GAS` is replaced by the
  measured difference.
- **The deployed executor is retired, not destroyed.** After the switch the
  owner revokes the trader on it; the owner can still `sweep` it.

## What changes off-chain

- `apex-exec`: an encoder for the new plan, with a cross-checked fixture both
  sides read and neither writes, as `PlanCommitmentFixture` is today.
- `live::calls`: venue reachability can no longer be read from an adapter
  registry; the venues the contract supports are a property of its code.
- The gas model: `GAS_PER_HOP`, the fixed overhead and the lender's extra gas,
  re-measured for the new contract.
- `canary::chain`: the new profit event; `EARLY_REVERT_GAS` re-checked (today's
  deadline reverts cost ~68k).
- Configuration: the executor's address and code hash in
  `ops/canary.base.yaml` and `ops/shadow.base.yaml`.
- The canary's boot check: the profit recipient alone.
- Owner transactions, prepared and dry-run here and broadcast by the owner:
  deploy, authorize the trader, set the recipient, seed dust, then revoke the
  trader on the old executor.

## Left to the builder

The plan's exact byte layout and the function names; how callback validation is
built (transient storage is the recommendation); the compiler version for the
new contract within the repository's Foundry setup; the lender default once
measured; the test structure, which needs at least: per-venue swap and callback
tests, callback spoofing from outside a trade, the deadline, the floor, the
circuit breaker, trader revocation, the encoder fixture, a gas test pinned to the
measured numbers, and the fork proof.

## How success is measured

- On a fork at block 52,671,989: ticket 664's route through the new contract,
  its gas used against 566,972 and its gross to the wei.
- The fork proof through the canary's own path, as in R28.
- Live: the canary's `gas_used` on every send, and its bid per gas, in the
  ledger it already keeps.
