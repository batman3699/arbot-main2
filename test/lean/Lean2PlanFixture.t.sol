// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {LeanPlan2Codec} from "../../contracts/lean/LeanPlan2Codec.sol";

/// The second executor's decoder, over calldata as `run` reads it.
contract Codec2Harness {
    function header(bytes calldata p) external pure returns (uint8, uint8, uint256, uint256, uint256) {
        return LeanPlan2Codec.header(p);
    }

    function hopAt(bytes calldata p, uint256 offset)
        external
        pure
        returns (uint8, address, address, uint256, uint24, int24, uint256)
    {
        return LeanPlan2Codec.hopAt(p, offset);
    }
}

/// Task 8.5 R37: `LeanPlan2Codec` reads every plan in
/// `crates/apex-exec/tests/fixtures/lean2_plans.json` back to its fields, as
/// `apex_exec::lean2` encodes them (`crates/apex-exec/tests/lean2_plan.rs`).
/// The fixture is read here, never written.
contract Lean2PlanFixtureTest is Test {
    Codec2Harness codec = new Codec2Harness();

    function testEveryFixturePlanDecodesToItsFields() public view {
        string memory json = vm.readFile("./crates/apex-exec/tests/fixtures/lean2_plans.json");
        string[] memory names = vm.parseJsonStringArray(json, ".names");
        assertEq(names.length, 6);
        for (uint256 n; n < names.length; n++) {
            string memory c = string.concat(".cases.", names[n]);
            bytes memory encoded = vm.parseJsonBytes(json, string.concat(c, ".encoded"));
            (uint8 lender, uint8 start, uint256 amount, uint256 deadline, uint256 minProfit) = codec.header(encoded);
            assertEq(lender, vm.parseJsonUint(json, string.concat(c, ".lender")), names[n]);
            assertEq(start, vm.parseJsonUint(json, string.concat(c, ".start")), names[n]);
            assertEq(amount, vm.parseJsonUint(json, string.concat(c, ".amount")), names[n]);
            assertEq(deadline, vm.parseJsonUint(json, string.concat(c, ".deadline")), names[n]);
            assertEq(minProfit, vm.parseJsonUint(json, string.concat(c, ".min_profit")), names[n]);
            uint256 hops = vm.parseJsonUint(json, string.concat(c, ".hop_count"));
            uint256 offset = LeanPlan2Codec.HEADER;
            for (uint256 i; i < hops; i++) {
                string memory h = string.concat(c, ".hops[", vm.toString(i), "]");
                (uint8 kind, address pool, address tokenOut, uint256 minOut, uint24 fee, int24 spacing, uint256 next) =
                    codec.hopAt(encoded, offset);
                assertEq(kind, vm.parseJsonUint(json, string.concat(h, ".kind")), names[n]);
                assertEq(pool, vm.parseJsonAddress(json, string.concat(h, ".pool")), names[n]);
                assertEq(tokenOut, vm.parseJsonAddress(json, string.concat(h, ".token_out")), names[n]);
                assertEq(minOut, vm.parseJsonUint(json, string.concat(h, ".min_out")), names[n]);
                if (kind == 3) {
                    assertEq(pool, vm.parseJsonAddress(json, string.concat(h, ".hooks")), names[n]);
                    assertEq(fee, vm.parseJsonUint(json, string.concat(h, ".fee")), names[n]);
                    assertEq(int256(spacing), vm.parseJsonInt(json, string.concat(h, ".tick_spacing")), names[n]);
                    assertEq(next, offset + LeanPlan2Codec.HOP + LeanPlan2Codec.V4_EXTRA, names[n]);
                } else {
                    assertEq(fee, 0, names[n]);
                    assertEq(spacing, 0, names[n]);
                    assertEq(next, offset + LeanPlan2Codec.HOP, names[n]);
                }
                offset = next;
            }
            assertEq(offset, encoded.length, string.concat(names[n], ": every byte read"));
        }
    }

    function testAHopPastTheEndIsRefused() public {
        bytes memory encoded = vm.parseJsonBytes(
            vm.readFile("./crates/apex-exec/tests/fixtures/lean2_plans.json"), ".cases.usdc_pool_manager_v4_v3.encoded"
        );
        vm.expectRevert();
        codec.hopAt(encoded, encoded.length - 10);
    }
}
