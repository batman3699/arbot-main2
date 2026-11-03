// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {BadPlan} from "./LeanPlanCodec.sol";

/// One hop of a plan-3 plan. `fee` and `tickSpacing` are 0 where the executor
/// reads its own.
struct Hop3 {
    uint8 kind;
    address pool;
    address tokenOut;
    uint24 fee;
    int24 tickSpacing;
    uint16 crossGas;
}

struct Plan3 {
    uint8 lender;
    uint8 start;
    uint256 cap;
    uint256 deadline;
    uint256 rate;
    uint256 gasOverhead;
    uint256 swapGas;
    uint256 l1Bound;
    uint256 minNet;
    uint256 maxSteps;
    Hop3[2] hops;
}

/// The third lean executor's plan (Task 8.5 R40.1): big-endian, packed, two
/// hops, always 162 bytes. It names no size and no hop minimums: the executor
/// sizes the trade at execution.
///
/// | Offset | Size | Field |
/// |--------|------|-------|
/// | 0      | 1    | lender: 0 Balancer V2, 1 Morpho Blue, 2 Uniswap v4's PoolManager |
/// | 1      | 1    | start: 0 WETH, 1 USDC |
/// | 2      | 16   | cap, in the start token |
/// | 18     | 5    | deadline: unix seconds |
/// | 23     | 16   | rate: start units per 1e18 wei, rounded up; 0 for WETH |
/// | 39     | 4    | gasOverhead |
/// | 43     | 4    | swapGas |
/// | 47     | 8    | l1Bound, wei |
/// | 55     | 8    | minNet, in the start token |
/// | 63     | 1    | maxSteps: 1 to 16 |
/// | 64 + 49i | 1  | hop kind: 0 Uniswap v3, 1 PancakeSwap v3, 2 Aerodrome v2, 3 Uniswap v4, 4 Slipstream |
/// | +1     | 20   | hop pool; for kind 3 the pool key's hooks |
/// | +21    | 20   | hop tokenOut; tokenIn is the start for hop 0, hop 0's tokenOut for hop 1 |
/// | +41    | 3    | fee, ppm, for kinds 0, 1 and 3 (the key's) |
/// | +44    | 3    | tickSpacing, two's complement |
/// | +47    | 2    | crossGas: one tick crossed on this hop |
///
/// `apex_exec::lean3` encodes the plan, and
/// `crates/apex-exec/tests/fixtures/lean3_plans.json` holds the two to each other.
library LeanPlan3Codec {
    uint256 internal constant LENGTH = 162;
    uint256 internal constant HEADER = 64;
    uint256 internal constant HOP = 49;
    uint256 internal constant MAX_STEPS_LIMIT = 16;

    /// The plan, its shape checked: a lender, a start token and kinds the
    /// executor has, a v4 hop only under PoolManager, and a step limit in range.
    function decode(bytes calldata p) internal pure returns (Plan3 memory plan) {
        if (p.length != LENGTH) revert BadPlan();
        plan.lender = uint8(p[0]);
        plan.start = uint8(p[1]);
        plan.cap = uint128(bytes16(p[2:18]));
        plan.deadline = uint40(bytes5(p[18:23]));
        plan.rate = uint128(bytes16(p[23:39]));
        plan.gasOverhead = uint32(bytes4(p[39:43]));
        plan.swapGas = uint32(bytes4(p[43:47]));
        plan.l1Bound = uint64(bytes8(p[47:55]));
        plan.minNet = uint64(bytes8(p[55:63]));
        plan.maxSteps = uint8(p[63]);
        if (plan.lender > 2 || plan.start > 1 || plan.maxSteps == 0 || plan.maxSteps > MAX_STEPS_LIMIT) {
            revert BadPlan();
        }
        for (uint256 i; i < 2; ++i) {
            uint256 o = HEADER + HOP * i;
            Hop3 memory h = plan.hops[i];
            h.kind = uint8(p[o]);
            if (h.kind > 4 || (h.kind == 3 && plan.lender != 2)) revert BadPlan();
            h.pool = address(bytes20(p[o + 1:o + 21]));
            h.tokenOut = address(bytes20(p[o + 21:o + 41]));
            h.fee = uint24(bytes3(p[o + 41:o + 44]));
            h.tickSpacing = int24(uint24(bytes3(p[o + 44:o + 47])));
            h.crossGas = uint16(bytes2(p[o + 47:o + 49]));
        }
    }
}
