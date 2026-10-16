// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// A plan the executor cannot read exactly.
error BadPlan();

/// The lean executor's plan (Task 8.5 R29): big-endian, packed, no padding.
///
/// | Offset    | Size | Field |
/// |-----------|------|-------|
/// | 0         | 1    | lender: 0 Balancer V2, 1 Morpho Blue |
/// | 1         | 16   | amount: WETH borrowed |
/// | 17        | 5    | deadline: unix seconds |
/// | 22        | 16   | minProfit |
/// | 38 + 57i  | 1    | hop kind: 0 uniswapV3SwapCallback pools, 1 pancakeV3SwapCallback pools, 2 Aerodrome v2 |
/// | +1        | 20   | hop pool |
/// | +21       | 20   | hop tokenOut; tokenIn is the previous hop's tokenOut, WETH for hop 0 |
/// | +41       | 16   | hop minOut: the exact quote; for kind 2 also the amount asked for |
///
/// `apex_exec::lean` encodes it, and `crates/apex-exec/tests/fixtures/lean_plans.json`
/// holds the two to each other.
library LeanPlanCodec {
    uint256 internal constant HEADER = 38;
    uint256 internal constant HOP = 57;

    function header(bytes calldata p)
        internal
        pure
        returns (uint8 lender, uint256 amount, uint256 deadline, uint256 minProfit, uint256 hopCount)
    {
        if (p.length < HEADER + HOP || (p.length - HEADER) % HOP != 0) revert BadPlan();
        lender = uint8(p[0]);
        amount = uint128(bytes16(p[1:17]));
        deadline = uint40(bytes5(p[17:22]));
        minProfit = uint128(bytes16(p[22:38]));
        hopCount = (p.length - HEADER) / HOP;
    }

    function hop(bytes calldata p, uint256 i)
        internal
        pure
        returns (uint8 kind, address pool, address tokenOut, uint256 minOut)
    {
        uint256 o = HEADER + HOP * i;
        kind = uint8(p[o]);
        pool = address(bytes20(p[o + 1:o + 21]));
        tokenOut = address(bytes20(p[o + 21:o + 41]));
        minOut = uint128(bytes16(p[o + 41:o + 57]));
    }
}
