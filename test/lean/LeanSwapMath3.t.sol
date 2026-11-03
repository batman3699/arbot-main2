// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {LeanSwapMath3} from "../../contracts/lean/LeanSwapMath3.sol";
import {TickMath} from "../../contracts/libraries/TickMath.sol";

/// Task 8.5 R40.1: `LeanSwapMath3` and `TickMath` against every case in
/// `crates/apex-exec/tests/fixtures/swap_steps3.json`, which
/// `apex_math::cl_math` wrote (`crates/apex-math/tests/swap_steps3.rs`).
contract LeanSwapMath3Test is Test {
    string json;

    function setUp() public {
        json = vm.readFile("./crates/apex-exec/tests/fixtures/swap_steps3.json");
    }

    function u(string memory path) internal view returns (uint256) {
        return vm.parseJsonUint(json, path);
    }

    function testEveryStepEqualsRusts() public view {
        for (uint256 i; i < 200; i++) {
            string memory c = string.concat(".steps[", vm.toString(i), "]");
            (uint160 next, uint256 amountIn, uint256 amountOut, uint256 feeAmount) = LeanSwapMath3.computeSwapStep(
                uint160(u(string.concat(c, ".current"))),
                uint160(u(string.concat(c, ".target"))),
                uint128(u(string.concat(c, ".liquidity"))),
                u(string.concat(c, ".remaining")),
                uint24(u(string.concat(c, ".fee")))
            );
            assertEq(next, u(string.concat(c, ".next")), c);
            assertEq(amountIn, u(string.concat(c, ".amount_in")), c);
            assertEq(amountOut, u(string.concat(c, ".amount_out")), c);
            assertEq(feeAmount, u(string.concat(c, ".fee_amount")), c);
        }
    }

    function testSqrtPricesAtTicksEqualRusts() public view {
        for (uint256 i; i < 40; i++) {
            string memory c = string.concat(".ticks[", vm.toString(i), "]");
            int24 tick = int24(vm.parseJsonInt(json, string.concat(c, ".tick")));
            assertEq(TickMath.getSqrtRatioAtTick(tick), u(string.concat(c, ".sqrt")), c);
        }
    }

    function testRootsEqualRusts() public view {
        for (uint256 i; i < 30; i++) {
            string memory c = string.concat(".roots[", vm.toString(i), "]");
            assertEq(LeanSwapMath3.sqrt(u(string.concat(c, ".x"))), u(string.concat(c, ".root")), c);
        }
    }

    function testMulDivUpRoundsOnlyARemainder() public pure {
        assertEq(LeanSwapMath3.mulDivUp(6, 4, 3), 8);
        assertEq(LeanSwapMath3.mulDivUp(7, 4, 3), 10);
        assertEq(LeanSwapMath3.mulDivUp(type(uint256).max, 2, 4), type(uint256).max / 2 + 1);
    }
}
