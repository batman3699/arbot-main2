# Executor ABI V2

## Overview

ABI V2 introduces versioned plan encoding for the executor and router so off-chain callers can opt into richer loan metadata without breaking legacy flows. Legacy calls remain available through `start(PlanLegacy)`/`startLegacy`, while new flows use `startV2(PlanV2)`.

## Types

- `enum LoanProvider { BALANCER, AAVE, ERC3156, UNIV2, UNIV3 }` (existing ordinals preserved; new providers appended)
- `struct Loan { address token; uint256 amount; LoanProvider provider; address providerAddr; }`
- `struct PlanV2 { Loan[] loans; uint16 cycleSlippageBps; Step[] steps; uint256 minProfit; }`
- `struct PlanLegacy { address loanToken; uint256 amountIn; LoanProvider loanProvider; uint16 cycleSlippageBps; Step[] steps; uint256 minProfit; }`
- `struct Step { Op op; bytes data; }` with `Op` ordering fixed at `UNIV3=0, BALANCER=1, GENERIC=2, BRIDGE=3, JIT_LP_ADD=4, JIT_LP_REMOVE=5`.

## Entrypoints

- `startV2(PlanV2)` – accepts version-prefixed calldata `(uint8(2), abi.encode(plan))`, enforces at least one loan (currently constrained to one) and supports ERC3156/Uniswap V2 flashswap/Uniswap V3 flash lenders via `providerAddr`.
- `start(PlanLegacy)`/`startLegacy(PlanLegacy)` – legacy single-loan path using version `1` context `(uint8(1), abi.encode(plan))`.
- `BatchRouter` exposes both `start` (legacy) and `startV2` and forwards to the executor.

## Compatibility & Migration

- Off-chain callers should migrate to `startV2` and populate `providerAddr` for ERC3156, UNIV2 pair, and UNIV3 pool loans. Balancer/Aave providers derive addresses on-chain.
- Legacy contexts must be wrapped in the versioned envelope; direct `abi.encode(plan)` payloads will revert in callbacks.
- On-chain validation enforces `cycleSlippageBps <= maxSlippageBps` and reverts with `InvalidMaxSlippage()` if violated. `minProfit` is checked only against realized post-repayment profit (`profitDelta`) and is no longer constrained by a notional-derived floor. Circuit gating remains manual pause-only (`tripCircuit` + cooldown / `resetCircuit`), with no on-chain loss-counter threshold auto-trip.


## Flash-loan callbacks

- Balancer path: `receiveFlashLoan(...)` validates sender against configured vault.
- Aave path: `executeOperation(...)` validates sender against configured pool and initiator = executor.
- ERC3156 path: `onFlashLoan(...)` validates sender/token/amount/context hash.
- Uniswap V2 path: `uniswapV2Call(...)` validates sender/context hash and computes fee using the standard `((amount * 3) / 997) + 1` formula before repayment to the pair.
- Uniswap V3 path: `uniswapV3FlashCallback(...)` validates sender/context hash and consumes `fee0/fee1` directly from the pool callback before repayment.
- Repayment behavior: Balancer/UNIV2/UNIV3 repay by direct token transfer back to lender, while Aave/ERC3156 repayment is allowance-based (ERC-3156 lenders pull via `transferFrom` after callback).
