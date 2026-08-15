# Executor modularization

- `contracts/executor/MultiVenueArbImplementation.sol` now contains core state management, flash-loan entrypoints/callbacks, config and profit distribution.
- Step dispatch in `_executeSteps` delegates to dedicated module contracts:
  - `contracts/executor/steps/SwapExecutor.sol`
  - `contracts/executor/steps/GenericExecutor.sol`
  - `contracts/executor/steps/JitExecutor.sol`
  - `contracts/executor/steps/BridgeExecutor.sol`
- Clone and router helpers were split into:
  - `contracts/executor/ArbitrageCloneFactory.sol`
  - `contracts/executor/BatchRouter.sol`
- Math utilities were split into standalone files under `contracts/libraries/`:
  - `FullMath.sol`
  - `TickMath.sol`
  - `LiquidityAmounts.sol`
- Bytecode guard is enforced with `scripts/ci/check_executor_size.sh` and `make check-contract-size`.

## Validation status (container)

- Foundry installed via `foundryup` in the container.
- `optimizer_runs` is set to `200` in `foundry.toml` to prioritize bytecode size headroom for deployment over micro-optimizing steady-state gas at high run counts.
- `forge inspect contracts/executor/MultiVenueArbImplementation.sol:MultiVenueArbImplementation deployedBytecode` now reports `22,620` runtime bytes (under the `24,576` EIP-170 cap).
- `scripts/ci/check_executor_size.sh` remains the CI guardrail to fail fast if future changes push runtime size over the cap.

## 2026-02 callback/code-size hardening

- Callback entrypoints (`receiveFlashLoan`, `executeOperation`, `onFlashLoan`, `uniswapV2Call`, `uniswapV3FlashCallback`) now share internal version-dispatch and active-loan validation helpers to remove duplicated revert branches and reduce runtime bytecode growth risk.
- String-based `require` usage in owner transfer and Balancer helper callback checks was replaced with custom errors for smaller revert payloads.
- Diagnostic `TokenApprovalSet` emissions from `_execJitAdd` were removed to avoid unnecessary high-frequency log writes; approval-change monitoring remains in explicit owner-driven `setTokenApprovals`.
- Test suite now includes an explicit runtime-size regression assertion for `MultiVenueArbImplementation` against the EIP-170 runtime cap (`< 24,576` bytes).
