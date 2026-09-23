// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Test} from "forge-std/Test.sol";
import {Deploy} from "../script/Deploy.s.sol";
import {BatchRouter} from "../contracts/executor/BatchRouter.sol";
import {MultiVenueArbImplementation} from "../contracts/executor/MultiVenueArbImplementation.sol";

/// B-13, fixed. Deploys from an explicit config rather than from the process
/// environment.
///
/// `testDeployTransfersRouterOwnershipToConfiguredExecutorOwner` used to set
/// `EXECUTOR_OWNER` and get `.env`'s `BASE_EXECUTOR_OWNER` instead — the
/// prefixed key correctly outranks the bare one, and no amount of care in the
/// test could change that. It is one of the four tests BASELINE.md recorded as
/// intermittently failing.
contract DeployOwnershipTest is Test {
    function _cfg(address configuredOwner) internal pure returns (Deploy.DeployConfig memory) {
        return Deploy.DeployConfig({
            prefix: "",
            vault: address(0x1111111111111111111111111111111111111111),
            uniRouter: address(0x2222222222222222222222222222222222222222),
            aavePool: address(0x3333333333333333333333333333333333333333),
            permit2: address(0x4444444444444444444444444444444444444444),
            configuredOwner: configuredOwner,
            privateKey: 0,
            salt: bytes32(uint256(0xC0FFEE)),
            requireBalancer: false,
            requireAave: false
        });
    }

    function testDeployInitialisesExecutorOwnerToRouter() external {
        Deploy deployScript = new Deploy();
        (,, address clone, BatchRouter router) = deployScript.runWith(_cfg(address(0xA11CE)));

        assertEq(
            MultiVenueArbImplementation(clone).owner(),
            address(router),
            "executor owner must be router"
        );
    }

    function testDeployTransfersRouterOwnershipToConfiguredExecutorOwner() external {
        address configuredOwner = makeAddr("configuredOwner");

        Deploy deployScript = new Deploy();
        (,, address clone, BatchRouter router) = deployScript.runWith(_cfg(configuredOwner));

        assertEq(
            MultiVenueArbImplementation(clone).owner(),
            address(router),
            "executor owner must remain router"
        );
        assertEq(router.owner(), configuredOwner, "router owner should be configured owner");
    }

    /// The ambient environment cannot change either answer. This is the test
    /// that would have failed before the seam existed.
    ///
    /// It deliberately perturbs two production keys. It does not touch `CHAIN`:
    /// that key has a reader (`DeployEnvPrefixTest`), and `vm.setEnv` writes the
    /// forge *process* environment, so two files contending for one key is
    /// precisely the B-13 mechanism. `scripts/ci/no_shared_env_keys.sh` holds
    /// the line. The keys below have no reader.
    function testDeployIgnoresTheAmbientEnvironment() external {
        address configuredOwner = makeAddr("explicitOwner");

        vm.setEnv("EXECUTOR_OWNER", vm.toString(address(0xDEAD)));
        vm.setEnv("BASE_EXECUTOR_OWNER", vm.toString(address(0xBEEF)));

        Deploy deployScript = new Deploy();
        (,,, BatchRouter router) = deployScript.runWith(_cfg(configuredOwner));

        assertEq(router.owner(), configuredOwner, "the environment reached an explicit deploy");
    }
}
