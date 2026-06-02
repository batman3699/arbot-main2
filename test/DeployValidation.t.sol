// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Test} from "forge-std/Test.sol";
import {Deploy} from "../script/Deploy.s.sol";

contract DeployHarness is Deploy {
    function validateResolvedIntegrations(
        string memory prefix,
        address vault,
        address uniRouter,
        address aavePool,
        address permit2
    ) external view {
        _validateResolvedIntegrations(prefix, vault, uniRouter, aavePool, permit2);
    }

    function isBalancerRequired(string memory prefix) external view returns (bool) {
        return _isBalancerRequired(prefix);
    }

    function isAaveRequired(string memory prefix) external view returns (bool) {
        return _isAaveRequired(prefix);
    }
}

contract DeployValidationTest is Test {
    uint256 internal constant INK_CHAIN_ID = 763373;

    DeployHarness internal deployHarness;

    function setUp() external {
        deployHarness = new DeployHarness();
    }

    function testRequireBalancerAddressWhenFlagEnabled() external {
        vm.chainId(INK_CHAIN_ID);
        vm.setEnv("REQUIRE_BALANCER", "true");

        vm.expectRevert(
            abi.encodeWithSelector(
                Deploy.MissingRequiredIntegration.selector,
                "BAL_VAULT|BALANCER_VAULT",
                INK_CHAIN_ID,
                ""
            )
        );

        deployHarness.validateResolvedIntegrations(
            "",
            address(0),
            makeAddr("univ3Router"),
            makeAddr("aavePool"),
            makeAddr("permit2")
        );
    }

    function testRequireAaveAddressWhenFlagEnabled() external {
        vm.chainId(INK_CHAIN_ID);
        vm.setEnv("REQUIRE_AAVE", "true");

        vm.expectRevert(
            abi.encodeWithSelector(Deploy.MissingRequiredIntegration.selector, "AAVE_POOL", INK_CHAIN_ID, "")
        );

        deployHarness.validateResolvedIntegrations(
            "",
            makeAddr("balancerVault"),
            makeAddr("univ3Router"),
            address(0),
            makeAddr("permit2")
        );
    }

    function testDefaultDoesNotRequireBalancerOrAave() external view {
        assertFalse(deployHarness.isBalancerRequired(""));
        assertFalse(deployHarness.isAaveRequired(""));
    }

    function testPrefixedRequirementFlagIsDetected() external {
        vm.setEnv("INK_REQUIRE_BALANCER", "true");
        vm.setEnv("INK_REQUIRE_AAVE", "true");

        assertTrue(deployHarness.isBalancerRequired("INK"));
        assertTrue(deployHarness.isAaveRequired("INK"));
    }
}
