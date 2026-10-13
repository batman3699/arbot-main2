# R27: a Morpho flash lender, so trades size past Balancer's 19 WETH

Task 8.5 R27. Status: design approved by the operator on 2026-10-13 (approach
A); this document is for review before the implementation plan.

## Goal

Size every trade, in both lanes, to its profit-maximising amount up to Morpho
Blue's WETH holding (~82,679 WETH), borrowed free, instead of stopping at
Balancer V2's (~19 WETH). Do it without changing the deployed executor.

## Evidence

**The cap binds where the money is.** Every loan comes from Balancer's vault,
which holds 18.98 WETH on Base (2026-10-13). The size search
(`apex_math::finite_size::best_size`) is otherwise unbounded, so the cap is the
one limit on sizing.

- **Armed arms at the cap** (R26.3 run, first 40 minutes):
  - 23% at threshold + 5;
  - 61% at + 20;
  - 98% at + 50;
  - 95% at + 100;
  - 100% at + 200.
- **The same 309 slots priced with an 18,198 WETH cap** (one block, 52,538,042, the shadow's own universe):

  | Rung | Rung surplus, Balancer only | Rung surplus, larger lender (after a 5 bps fee) |
  |---|---|---|
  | + 50 | $22,781 | $46,087 (+102%) |
  | + 100 | $57,040 | $172,771 (+203%) |
  | + 200 | $142,985 | $577,220 (+304%) |

  Per capped spike won, that is about +$240 at + 50, +$1,080 at + 100 and +$2,900 at + 200.
  - The figures are "if the move happens". Moves of 50 ticks or more came about once an hour across all pools.
  - One pool, Uniswap WETH/bsdETH `0xdea6…` (no liquidity, refused by admission), priced impossibly and was left out. The live engine has never armed it: 0 of 55,580 arms.
- **The reactive lane has not hit the cap yet.** Its largest ticket was 17.88 WETH, in the 21:00 UTC spike on 2026-10-12.

**Lenders on Base**, read 2026-10-13 and checked against chain code:

| Lender | WETH | Fee | Through the deployed executor |
|---|---|---|---|
| Balancer V2 vault | 18.98 | 0 | yes (today's lender) |
| Morpho Blue `0xBBBB…EEFFCb` | 82,679 | 0 | not natively; **yes through an ERC-3156 lender** |
| Aave v3 pool `0xA238…d1c5` | 18,198 | 5 bps | **no: settlement bug** |
| Uniswap v3 WETH/cbBTC 0.05% | 5,525 | 5 bps | by pool flash (not simulated); dominated by Morpho |

**The deployed executor cannot settle an Aave loan.**
- Aave pulls its repayment after `executeOperation` returns, but `_handleFlashLoanV2` treats it like a lender repaid by transfer. `_repay` only approves, then `_assertSettled(token, startBalance, 0, residue)` counts the principal and premium still held as profit.
- **Simulated on 2026-10-13** with a 1 WETH loan, no steps, and the executor's WETH balance set to 10 by override:
  - the Aave plan succeeded and emitted `ProfitRealised(WETH, 1.0 WETH, owner fee 0.03 WETH)`;
  - the Balancer control reverted, correctly, for zero profit.
- With the executor's real balance, every Aave-funded trade would revert. With a balance, the minimum-profit check would protect nothing.

**The executor's ERC-3156 branch settles correctly.**
- It names the lender in the plan, records it, and checks in `onFlashLoan` that:
  - the caller is that lender;
  - the initiator is itself;
  - token, amount and context hash match.
- It settles with `repayment = amount + fee`, distributes the profit, and approves the lender to pull `repay`.
- Morpho Blue's interface was verified in its bytecode: `flashLoan(address,uint256,bytes)` (selector `0xe0232b42`, sends the assets, calls `onMorphoFlashLoan(uint256,bytes)` on the caller, then pulls them back).

## Design

### 1. `MorphoFlashLender` (Solidity, `contracts/lenders/`)

An ERC-3156 lender over Morpho Blue: immutable, no owner, holding nothing between transactions.

- `constructor(IMorpho morpho)`; `morpho()` returns it.
- `maxFlashLoan(token)` = `token.balanceOf(morpho)`; `flashFee(token, amount)` = 0.
- `flashLoan(receiver, token, amount, data)`:
  - refuses while a loan is in flight;
  - records the receiver and the initiator (`msg.sender`);
  - calls `morpho.flashLoan(token, amount, abi.encode(token, data))`;
  - clears the record and returns `true`.
- `onMorphoFlashLoan(assets, d)`:
  - only `morpho` may call it, and only while a loan is in flight;
  - transfers `assets` to the receiver;
  - requires `receiver.onFlashLoan(initiator, token, assets, 0, data)` to return `keccak256("ERC3156FlashBorrower.onFlashLoan")`;
  - pulls `assets` back from the receiver (`transferFrom`) and approves Morpho for `assets`.
- Anyone may borrow through it: Morpho's flash loans are public and free already. A borrower that does not repay makes `transferFrom` fail, and the whole loan reverts.
- **Tests** (Foundry, offline):
  - a mock Morpho with Morpho Blue's flash-loan semantics, a mock WETH, and mock borrowers;
  - a round trip;
  - a callback not from Morpho, or outside a loan, refused;
  - re-entry refused;
  - a borrower that returns the wrong value, or does not approve, refused;
  - nothing left in the lender afterwards.
- **Mutation:** every guard.

### 2. Proof before deploying, on Base, by simulation

`eth_simulateV1` through the real deployed executor, the lender's runtime code injected by state override at a fixed address:

1. **A 1 WETH ERC-3156 loan with no steps,** the executor holding 10 WETH by override: it must revert for zero profit, unlike Aave.
2. **A real profitable cycle above 19 WETH,** at a fired arm's fire state (the evaluator's full-block replay): it must settle with `ProfitRealised` equal to the exact quote, to the wei.
3. **The gas difference against the same cycle on Balancer:** the Morpho overhead the gas model takes.

If any of these fails, the design stops here.

### 3. Deploy

- A Foundry script, dry-run on a fork. The operator broadcasts it; it needs no executor configuration.
- The address and the runtime code hash go in `ops/shadow.base.yaml` under `lenders.morpho`.
- At boot the shadow reads the code at that address and refuses to use it unless its hash matches and `morpho()` returns Morpho Blue. No address is trusted unread.

### 4. Both lanes (Rust)

- **Lenders.**
  - `Lender { id, kind, address, available, extra_gas }`.
  - Two of them: Balancer (`FlashProviderId(1)`, `LoanProvider::Balancer`) and Morpho (`FlashProviderId(2)`, `LoanProvider::ERC3156` with the wrapper's address).
  - Both are fee-free, so `FlashTerms`' fee-free-only rule still holds.
  - Each head reads both holdings in one multicall.
- **Choice by size:** the first lender, in order Balancer then Morpho, whose holding covers the size. Balancer first because it needs no wrapper hops.
- **Cap:** the size search's cap becomes the larger holding.
  - A size above Balancer's holding carries Morpho's `extra_gas`, measured in step 2.
  - That puts one small step into net(size), as gas crossings already do; `best_size` treats steps as it does today.
- **Armed lane:**
  - `Pricing.cap` = the larger holding;
  - `armed_call` builds the loan from the chosen lender;
  - the gas limit follows the shared formula with the overhead;
  - the arm line records the lender.
- **Reactive lane:**
  - `LiveEconomics` holds both lenders' terms, and `flash_quote` chooses by the refined amount;
  - the chosen provider rides the candidate to the commitment and to `LiveCallBuilder`, which maps it to the loan instead of refusing everything but Balancer;
  - the reader's check before signing re-reads the chosen lender's holding.
- **Aave:** the plan builders refuse `LoanProvider::Aave` for this executor, with a test; the bug is recorded in `PLAN.md`.

### 5. Rollout and success

1. Contract and tests; then the step 2 proofs. **Stop if any fails.**
2. The operator deploys; boot verification.
3. The Rust changes, all gates and mutations, a commit verified in a clean worktree, then the shadow redeploy (a new 14-day clock).
4. **Success:**
   - arms and reactive tickets sized above 19 WETH appear and are priced with Morpho;
   - the evaluator's simulations of Morpho-funded fires agree with their quotes;
   - nothing else regresses (reactive funnel, evaluator agreement on Balancer fires).

## Risks

- **Morpho's WETH can be withdrawn.** It is read every head and re-read before signing, as Balancer's is.
- **The wrapper is new code holding loans for one transaction.** Its tests, the mutations and the step 2 proofs bound that, and it holds nothing at rest.
- **Larger sizes cross more ticks.**
  - The gas model and the refund-aware limits (R26.1) price that.
  - A larger limit can land a flashblock later (INV-19). Arms wait in the builder; the reactive adapter already schedules by limit.
- **A larger trade moves both pools further,** and the exact quote already prices it. The evaluator checks fires against `eth_simulateV1`.

## Not in R27

- **A new executor** (Aave fixed, Morpho native). Not needed while the ERC-3156 path works. If one is ever deployed, fix the Aave branch to treat Aave as a pull-after-return lender.
- **Two loans in one plan.** The executor takes exactly one.
