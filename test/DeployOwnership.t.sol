// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Test} from "forge-std/Test.sol";
import {Deploy} from "../script/Deploy.s.sol";
import {BatchRouter} from "../contracts/executor/BatchRouter.sol";
import {MultiVenueArbImplementation} from "../contracts/executor/MultiVenueArbImplementation.sol";

contract DeployOwnershipTest is Test {
    function testDeployInitialisesExecutorOwnerToRouter() external {
        Deploy deployScript = new Deploy();
        (, , address clone, BatchRouter router) = deployScript.run();

        assertEq(MultiVenueArbImplementation(clone).owner(), address(router), "executor owner must be router");
    }

    function testDeployTransfersRouterOwnershipToConfiguredExecutorOwner() external {
        address configuredOwner = makeAddr("configuredOwner");
        vm.setEnv("EXECUTOR_OWNER", vm.toString(configuredOwner));

        Deploy deployScript = new Deploy();
        (, , address clone, BatchRouter router) = deployScript.run();

        assertEq(MultiVenueArbImplementation(clone).owner(), address(router), "executor owner must remain router");
        assertEq(router.owner(), configuredOwner, "router owner should be configured owner");
    }
}
