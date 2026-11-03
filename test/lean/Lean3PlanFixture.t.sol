// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {BadPlan} from "../../contracts/lean/LeanPlanCodec.sol";
import {LeanPlan3Codec, Plan3} from "../../contracts/lean/LeanPlan3Codec.sol";

/// The third executor's decoder, over calldata as `run` reads it.
contract Codec3Harness {
    function decode(bytes calldata p) external pure returns (Plan3 memory) {
        return LeanPlan3Codec.decode(p);
    }
}

/// Task 8.5 R40.1: `LeanPlan3Codec` reads every plan in
/// `crates/apex-exec/tests/fixtures/lean3_plans.json` back to its fields, as
/// `apex_exec::lean3` encodes them (`crates/apex-exec/tests/lean3_plan.rs`).
/// The fixture is read here, never written.
contract Lean3PlanFixtureTest is Test {
    Codec3Harness codec = new Codec3Harness();
    string json;

    function setUp() public {
        json = vm.readFile("./crates/apex-exec/tests/fixtures/lean3_plans.json");
    }

    function field(string memory c, string memory name) internal view returns (uint256) {
        return vm.parseJsonUint(json, string.concat(c, ".", name));
    }

    function testEveryFixturePlanDecodesToItsFields() public view {
        string[] memory names = vm.parseJsonStringArray(json, ".names");
        assertEq(names.length, 6);
        for (uint256 n; n < names.length; n++) {
            string memory c = string.concat(".cases.", names[n]);
            Plan3 memory p = codec.decode(vm.parseJsonBytes(json, string.concat(c, ".encoded")));
            assertEq(p.lender, field(c, "lender"), names[n]);
            assertEq(p.start, field(c, "start"), names[n]);
            assertEq(p.cap, field(c, "cap"), names[n]);
            assertEq(p.deadline, field(c, "deadline"), names[n]);
            assertEq(p.rate, field(c, "rate"), names[n]);
            assertEq(p.gasOverhead, field(c, "gas_overhead"), names[n]);
            assertEq(p.swapGas, field(c, "swap_gas"), names[n]);
            assertEq(p.l1Bound, field(c, "l1_bound"), names[n]);
            assertEq(p.minNet, field(c, "min_net"), names[n]);
            assertEq(p.maxSteps, field(c, "max_steps"), names[n]);
            for (uint256 i; i < 2; i++) {
                string memory h = string.concat(c, ".hops[", vm.toString(i), "]");
                assertEq(p.hops[i].kind, field(h, "kind"), names[n]);
                assertEq(p.hops[i].pool, vm.parseJsonAddress(json, string.concat(h, ".pool")), names[n]);
                assertEq(p.hops[i].tokenOut, vm.parseJsonAddress(json, string.concat(h, ".token_out")), names[n]);
                assertEq(p.hops[i].fee, field(h, "fee"), names[n]);
                assertEq(int256(p.hops[i].tickSpacing), vm.parseJsonInt(json, string.concat(h, ".tick_spacing")), names[n]);
                assertEq(p.hops[i].crossGas, field(h, "cross_gas"), names[n]);
            }
        }
    }

    function base() internal view returns (bytes memory) {
        return vm.parseJsonBytes(json, ".cases.weth_balancer_v3_pancake.encoded");
    }

    function expectBad(bytes memory p) internal {
        vm.expectRevert(BadPlan.selector);
        codec.decode(p);
    }

    function testALengthOtherThan162IsRefused() public {
        bytes memory p = base();
        bytes memory short = new bytes(161);
        bytes memory long = new bytes(163);
        for (uint256 i; i < 161; i++) {
            short[i] = p[i];
            long[i] = p[i];
        }
        long[161] = p[161];
        expectBad(short);
        expectBad(long);
    }

    function testMaxStepsOutOfRangeIsRefused() public {
        bytes memory p = base();
        p[63] = 0x00;
        expectBad(p);
        p[63] = 0x11;
        expectBad(p);
        p[63] = 0x10;
        assertEq(codec.decode(p).maxSteps, 16);
    }

    function testAnUnknownLenderStartOrKindIsRefused() public {
        bytes memory p = base();
        p[0] = 0x03;
        expectBad(p);
        p = base();
        p[1] = 0x02;
        expectBad(p);
        p = base();
        p[64 + 49] = 0x05;
        expectBad(p);
    }

    function testAV4HopOutsidePoolManagerIsRefused() public {
        bytes memory p = vm.parseJsonBytes(json, ".cases.usdc_pool_manager_v4_v3.encoded");
        assertEq(codec.decode(p).lender, 2);
        p[0] = 0x01;
        expectBad(p);
    }
}
