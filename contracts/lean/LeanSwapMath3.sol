// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {FullMath} from "../libraries/FullMath.sol";

/// The concentrated-liquidity swap step `LeanExecutor3` sizes and quotes with
/// (Task 8.5 R40.1): v3-core's `SwapMath.computeSwapStep` and the
/// `SqrtPriceMath` it uses, exact input only. `apex_math::cl_math` is the same
/// arithmetic, and `crates/apex-exec/tests/fixtures/swap_steps3.json` holds the
/// two to each other.
library LeanSwapMath3 {
    uint256 internal constant Q96 = 0x1000000000000000000000000;
    uint256 internal constant PPM = 1_000_000;

    function mulDivUp(uint256 a, uint256 b, uint256 d) internal pure returns (uint256 result) {
        result = FullMath.mulDiv(a, b, d);
        if (mulmod(a, b, d) > 0) result += 1;
    }

    function divUp(uint256 a, uint256 b) internal pure returns (uint256) {
        return a / b + (a % b > 0 ? 1 : 0);
    }

    /// `L · (√b − √a) / (√a · √b)`, Q96.
    function amount0Delta(uint160 a, uint160 b, uint128 liquidity, bool roundUp) internal pure returns (uint256) {
        if (a > b) (a, b) = (b, a);
        if (a == 0) revert();
        uint256 n1 = uint256(liquidity) << 96;
        uint256 n2 = b - a;
        return roundUp ? divUp(mulDivUp(n1, n2, b), a) : FullMath.mulDiv(n1, n2, b) / a;
    }

    /// `L · (√b − √a)`, Q96.
    function amount1Delta(uint160 a, uint160 b, uint128 liquidity, bool roundUp) internal pure returns (uint256) {
        if (a > b) (a, b) = (b, a);
        return roundUp ? mulDivUp(liquidity, b - a, Q96) : FullMath.mulDiv(liquidity, b - a, Q96);
    }

    /// The price after `amountIn` (net of fee) goes in: rounded up when token0
    /// goes in, down when token1 does, so the output is never overstated.
    function nextSqrtFromInput(uint160 sqrtP, uint128 liquidity, uint256 amountIn, bool zeroForOne)
        internal
        pure
        returns (uint160)
    {
        if (sqrtP == 0 || liquidity == 0) revert();
        if (amountIn == 0) return sqrtP;
        if (zeroForOne) {
            uint256 n1 = uint256(liquidity) << 96;
            unchecked {
                uint256 product = amountIn * sqrtP;
                if (product / amountIn == sqrtP) {
                    uint256 denominator = n1 + product;
                    if (denominator >= n1) return uint160(mulDivUp(n1, sqrtP, denominator));
                }
            }
            return uint160(divUp(n1, n1 / sqrtP + amountIn));
        }
        uint256 next = uint256(sqrtP) + FullMath.mulDiv(amountIn, Q96, liquidity);
        if (next > type(uint160).max) revert();
        return uint160(next);
    }

    /// One exact-input step from `current` towards `target` with `remaining` in.
    function computeSwapStep(uint160 current, uint160 target, uint128 liquidity, uint256 remaining, uint24 feePips)
        internal
        pure
        returns (uint160 next, uint256 amountIn, uint256 amountOut, uint256 feeAmount)
    {
        bool zeroForOne = current >= target;
        uint256 keep = PPM - feePips;
        uint256 lessFee = FullMath.mulDiv(remaining, keep, PPM);
        amountIn = zeroForOne ? amount0Delta(target, current, liquidity, true) : amount1Delta(current, target, liquidity, true);
        if (lessFee >= amountIn) {
            next = target;
        } else {
            next = nextSqrtFromInput(current, liquidity, lessFee, zeroForOne);
        }
        bool reached = next == target;
        if (zeroForOne) {
            if (!reached) amountIn = amount0Delta(next, current, liquidity, true);
            amountOut = amount1Delta(next, current, liquidity, false);
        } else {
            if (!reached) amountIn = amount1Delta(current, next, liquidity, true);
            amountOut = amount0Delta(current, next, liquidity, false);
        }
        feeAmount = reached ? mulDivUp(amountIn, feePips, keep) : remaining - amountIn;
    }

    /// The integer square root, rounded down: Newton's method from a power of
    /// two above the root, which takes a handful of steps where x / 2 takes a
    /// hundred.
    function sqrt(uint256 x) internal pure returns (uint256 z) {
        if (x == 0) return 0;
        z = 1 << ((log2(x) >> 1) + 1);
        uint256 y = (z + x / z) >> 1;
        while (y < z) {
            z = y;
            y = (z + x / z) >> 1;
        }
    }

    /// The index of `x`'s highest set bit; `x` is not 0.
    function log2(uint256 x) internal pure returns (uint256 r) {
        if (x >= 1 << 128) {
            x >>= 128;
            r += 128;
        }
        if (x >= 1 << 64) {
            x >>= 64;
            r += 64;
        }
        if (x >= 1 << 32) {
            x >>= 32;
            r += 32;
        }
        if (x >= 1 << 16) {
            x >>= 16;
            r += 16;
        }
        if (x >= 1 << 8) {
            x >>= 8;
            r += 8;
        }
        if (x >= 1 << 4) {
            x >>= 4;
            r += 4;
        }
        if (x >= 1 << 2) {
            x >>= 2;
            r += 2;
        }
        if (x >= 1 << 1) r += 1;
    }
}
