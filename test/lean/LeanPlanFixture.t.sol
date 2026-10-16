// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {LeanPlanCodec} from "../../contracts/lean/LeanPlanCodec.sol";

/// The contract's decoder, over calldata as `run` reads it.
contract CodecHarness {
    function header(bytes calldata p) external pure returns (uint8, uint256, uint256, uint256, uint256) {
        return LeanPlanCodec.header(p);
    }

    function hop(bytes calldata p, uint256 i) external pure returns (uint8, address, address, uint256) {
        return LeanPlanCodec.hop(p, i);
    }
}

/// Task 8.5 R29: `LeanPlanCodec` reads every plan in
/// `crates/apex-exec/tests/fixtures/lean_plans.json` back to its fields, as
/// `apex_exec::lean` encodes them (`crates/apex-exec/tests/lean_plan.rs`). The
/// fixture is read here, never written.
contract LeanPlanFixtureTest is Test {
    CodecHarness codec = new CodecHarness();

    function testEveryFixturePlanDecodesToItsFields() public view {
        string memory json = vm.readFile("./crates/apex-exec/tests/fixtures/lean_plans.json");
        string[] memory names = vm.parseJsonStringArray(json, ".names");
        assertEq(names.length, 3);
        for (uint256 n; n < names.length; n++) {
            string memory c = string.concat(".cases.", names[n]);
            bytes memory encoded = vm.parseJsonBytes(json, string.concat(c, ".encoded"));
            (uint8 lender, uint256 amount, uint256 deadline, uint256 minProfit, uint256 hops) = codec.header(encoded);
            assertEq(lender, vm.parseJsonUint(json, string.concat(c, ".lender")), names[n]);
            assertEq(amount, vm.parseJsonUint(json, string.concat(c, ".amount")), names[n]);
            assertEq(deadline, vm.parseJsonUint(json, string.concat(c, ".deadline")), names[n]);
            assertEq(minProfit, vm.parseJsonUint(json, string.concat(c, ".min_profit")), names[n]);
            assertEq(hops, vm.parseJsonUint(json, string.concat(c, ".hop_count")), names[n]);
            for (uint256 i; i < hops; i++) {
                string memory h = string.concat(c, ".hops[", vm.toString(i), "]");
                (uint8 kind, address pool, address tokenOut, uint256 minOut) = codec.hop(encoded, i);
                assertEq(kind, vm.parseJsonUint(json, string.concat(h, ".kind")), names[n]);
                assertEq(pool, vm.parseJsonAddress(json, string.concat(h, ".pool")), names[n]);
                assertEq(tokenOut, vm.parseJsonAddress(json, string.concat(h, ".token_out")), names[n]);
                assertEq(minOut, vm.parseJsonUint(json, string.concat(h, ".min_out")), names[n]);
            }
        }
    }
}
