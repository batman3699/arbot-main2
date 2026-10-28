// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {BadPlan} from "./LeanPlanCodec.sol";

/// The second lean executor's plan (Task 8.5 R37): big-endian, packed, no
/// padding.
///
/// | Offset | Size | Field |
/// |--------|------|-------|
/// | 0      | 1    | lender: 0 Balancer V2, 1 Morpho Blue, 2 Uniswap v4's PoolManager |
/// | 1      | 1    | start: 0 WETH, 1 USDC |
/// | 2      | 16   | amount borrowed, in the start token |
/// | 18     | 5    | deadline: unix seconds |
/// | 23     | 16   | minProfit, in the start token |
/// | 39 + … | 1    | hop kind: 0 uniswapV3SwapCallback pools, 1 pancakeV3SwapCallback pools, 2 Aerodrome v2, 3 Uniswap v4 |
/// | +1     | 20   | hop pool; for kind 3 the pool key's hooks |
/// | +21    | 20   | hop tokenOut; tokenIn is the previous hop's tokenOut, the start for hop 0 |
/// | +41    | 16   | hop minOut: the exact quote; for kind 2 also the amount asked for |
/// | +57    | 3    | kind 3 only: the pool key's fee |
/// | +60    | 3    | kind 3 only: the pool key's tickSpacing, two's complement |
///
/// Hops are read in order: a kind-3 hop is six bytes longer. `apex_exec::lean2`
/// encodes the plan, and `crates/apex-exec/tests/fixtures/lean2_plans.json`
/// holds the two to each other.
library LeanPlan2Codec {
    uint256 internal constant HEADER = 39;
    uint256 internal constant HOP = 57;
    uint256 internal constant V4_EXTRA = 6;

    function header(bytes calldata p)
        internal
        pure
        returns (uint8 lender, uint8 start, uint256 amount, uint256 deadline, uint256 minProfit)
    {
        if (p.length < HEADER + HOP) revert BadPlan();
        lender = uint8(p[0]);
        start = uint8(p[1]);
        amount = uint128(bytes16(p[2:18]));
        deadline = uint40(bytes5(p[18:23]));
        minProfit = uint128(bytes16(p[23:39]));
    }

    /// The hop at `offset`, and the offset of the next. `fee` and
    /// `tickSpacing` are zero for every kind but 3.
    function hopAt(bytes calldata p, uint256 offset)
        internal
        pure
        returns (
            uint8 kind,
            address pool,
            address tokenOut,
            uint256 minOut,
            uint24 fee,
            int24 tickSpacing,
            uint256 next
        )
    {
        next = offset + HOP;
        if (next > p.length) revert BadPlan();
        kind = uint8(p[offset]);
        pool = address(bytes20(p[offset + 1:offset + 21]));
        tokenOut = address(bytes20(p[offset + 21:offset + 41]));
        minOut = uint128(bytes16(p[offset + 41:offset + 57]));
        if (kind == 3) {
            if (next + V4_EXTRA > p.length) revert BadPlan();
            fee = uint24(bytes3(p[next:next + 3]));
            tickSpacing = int24(uint24(bytes3(p[next + 3:next + 6])));
            next += V4_EXTRA;
        }
    }
}
