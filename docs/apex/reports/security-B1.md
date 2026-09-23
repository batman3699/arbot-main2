# B-1 — arbitrary call and arbitrary approval in `_execGeneric`

**Status:** demonstrated 2026-09-24 by `test/NoArbitraryCall.t.sol`, five passing tests.
**Severity:** Critical for a funded contract. **Currently unexposed** — see below.
**Fix:** Phase 5 Task 5.1. `AdapterRegistry` + typed steps; `_execGeneric` and
`_execModule` deleted.

## What the code does

`contracts/executor/MultiVenueArbImplementation.sol`:

```solidity
function _execGeneric(bytes memory data) internal {
    (address target, bytes memory callData, uint256 action, address token, uint256 amount) =
        abi.decode(data, (address, bytes, uint256, address, uint256));
    if (action == 1) {
        _ensureDirectAllowance(token, target, amount);   // <- standing approval
    } else if (action == 2) {
        _safeTransfer(token, target, amount);            // <- outright transfer
    }
    target.safeCall(callData);                           // <- arbitrary call
}
```

`target`, `callData`, `token` and `amount` all come from the step payload. There
is no allowlist, no adapter identity, and no check that `target` is a venue.

Reached by `startV2` → `_initiateLoanV2` → `_executeSteps` → `Op.GENERIC` →
`genericExecutorModule.delegatecall(...)` → `moduleExecGeneric` →
`_execGeneric`.

## What was measured, and what was wrong in the original finding

The finding recorded in PLAN.md §3.4 was "arbitrary call surface". That is true
and incomplete, and the incompleteness runs in both directions.

| Path | Result | Evidence |
|---|---|---|
| `action == 0`, arbitrary call | **Reachable.** The call executes with the executor's identity. | `testGenericStepCanCallAnyTarget` |
| `action == 2`, transfer out | **Caught.** `InsufficientFinalBalance` fires at settlement and the transaction reverts; the transfer is undone. | `testGenericStepTransferIsCaughtByTheProfitInvariant` |
| `action == 1`, grant allowance | **Reachable and permanent.** No tokens move, so the final-balance check sees nothing; the settlement succeeds and the approval outlives it. | `testGenericStepCanGrantAllowanceToAnyAddress`, `testTheGrantedAllowanceDrainsTheExecutorLater` |

The first version of the transfer test asserted the theft completed. It failed
with `InsufficientFinalBalance(1.6e19, 2.1e19)` — the profit invariant working
exactly as designed. That correction matters: it is the difference between
"this contract can be emptied in one transaction" and the truth.

**The unmitigated hole is the approval.** A single successful settlement can
leave behind an unlimited standing claim on every token balance the contract
will ever hold, drainable later, from any address, with no executor key
involved. `testTheGrantedAllowanceDrainsTheExecutorLater` completes that drain
in a second transaction.

## Why `onlyExecutor` does not resolve it

Every entry point (`start`, `startLegacy`, `startV2`) is `onlyExecutor`, and an
unauthorised caller cannot reach any of this — asserted by
`testAnUnauthorisedCallerCannotReachTheGenericStep`. So this is not open to the
public.

It remains the highest-severity item in the plan because **an executor is
authorised to trade, not to transfer.** A settlement contract exists so that the
key which routes trades cannot also empty the account. `_execGeneric` collapses
that distinction, and every control above it — signer separation, capital
limits, the risk ladder — rests on a boundary the contract does not enforce.

## Current exposure

**None, today.** The deployed executor
`0xDbFB219b4F1CE08fA61C5cD3c08C1307760cAec6` holds 0 ETH and no token balances,
and no live run has occurred. This is also why PLAN.md's R-03 says not to fund
it beyond canary size until Phase 5 replaces `_execGeneric`.

This document is published in a public repository alongside the vulnerable
source, which has been public throughout. The analysis does not create exposure
that the source did not already carry, and the contract it describes holds
nothing.

## What the fix has to preserve

`_execGeneric` is not dead code being removed for tidiness — it is **how the
existing tests move value**. `test/MultiVenueArbExecutor.t.sol`'s `_profitStep`
builds a `GENERIC` step to call a `ProfitDonor`, and that is the normal path by
which a test plan realises profit. Deleting the surface without replacing it
breaks the settlement mechanism.

So Task 5.1 is not a deletion. It is a substitution: `AdapterRegistry` plus a
typed `Step { adapterId, poolKey, payload }`, where the adapter set is
enumerated and the target is resolved by identity rather than supplied by the
caller.

## Inversion

When the fix lands, these five tests become
`testNoArbitraryCallSurfaceExists`, asserting the attacker call reverts with
`UnknownAdapter`, and `scripts/ci/check_no_generic_call.sh` is wired — source
grep plus a runtime bytecode scan, which is the part that catches a
reintroduction through a different name.
