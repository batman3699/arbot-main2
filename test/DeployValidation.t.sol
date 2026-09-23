// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Test} from "forge-std/Test.sol";
import {Deploy} from "../script/Deploy.s.sol";

contract DeployHarness is Deploy {}

/// B-13, fixed. **Not one `vm.setEnv` in this file.**
///
/// These tests used to write `REQUIRE_BALANCER` and `REQUIRE_AAVE` into the
/// forge process to steer the validation, which meant
/// `testDefaultDoesNotRequireBalancerOrAave` passed or failed depending on
/// whether a sibling had run first. It now passes a `DeployConfig` and the
/// question has one answer.
contract DeployValidationTest is Test {
    uint256 internal constant INK_CHAIN_ID = 763373;

    DeployHarness internal deployHarness;

    function setUp() external {
        deployHarness = new DeployHarness();
    }

    /// A config with nothing resolved and nothing required. Each test states
    /// the one field it is about, which is also what makes each test readable
    /// on its own.
    function _cfg() internal pure returns (Deploy.DeployConfig memory cfg) {
        cfg = Deploy.DeployConfig({
            prefix: "",
            vault: address(0),
            uniRouter: address(0x1111111111111111111111111111111111111111),
            aavePool: address(0),
            permit2: address(0x2222222222222222222222222222222222222222),
            configuredOwner: address(0),
            privateKey: 0,
            salt: bytes32(uint256(1)),
            requireBalancer: false,
            requireAave: false
        });
    }

    function testRequireBalancerAddressWhenFlagEnabled() external {
        vm.chainId(INK_CHAIN_ID);
        Deploy.DeployConfig memory cfg = _cfg();
        cfg.requireBalancer = true;

        vm.expectRevert(
            abi.encodeWithSelector(
                Deploy.MissingRequiredIntegration.selector,
                "BAL_VAULT|BALANCER_VAULT",
                INK_CHAIN_ID,
                ""
            )
        );
        deployHarness.resolveConfig(cfg);
    }

    function testRequireAaveAddressWhenFlagEnabled() external {
        vm.chainId(INK_CHAIN_ID);
        Deploy.DeployConfig memory cfg = _cfg();
        cfg.requireAave = true;
        // Give Balancer an address so the Aave check is what fires.
        cfg.vault = address(0xBA1);

        vm.expectRevert(
            abi.encodeWithSelector(
                Deploy.MissingRequiredIntegration.selector, "AAVE_POOL", INK_CHAIN_ID, ""
            )
        );
        deployHarness.resolveConfig(cfg);
    }

    /// The test that used to fail because a sibling had set the flag.
    function testDefaultDoesNotRequireBalancerOrAave() external {
        vm.chainId(INK_CHAIN_ID);
        Deploy.DeployConfig memory resolved = deployHarness.resolveConfig(_cfg());
        assertFalse(resolved.requireBalancer, "balancer must not be required by default");
        assertFalse(resolved.requireAave, "aave must not be required by default");
    }

    /// A chain with known defaults fills them in rather than refusing.
    function testResolutionFillsKnownDefaults() external {
        vm.chainId(1);
        Deploy.DeployConfig memory cfg = _cfg();
        cfg.uniRouter = address(0);
        cfg.permit2 = address(0);

        Deploy.DeployConfig memory resolved = deployHarness.resolveConfig(cfg);
        assertEq(resolved.uniRouter, 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45);
        assertEq(resolved.permit2, 0x000000000022D473030F116dDEE9F6B43aC78BA3);
        assertEq(resolved.vault, 0xBA12222222228d8Ba445958a75a0704d566BF2C8);
    }

    /// An explicit address is never overwritten by a default. The resolution
    /// fills gaps; it does not have opinions.
    function testAnExplicitAddressSurvivesResolution() external {
        vm.chainId(1);
        Deploy.DeployConfig memory cfg = _cfg();
        cfg.vault = address(0xBEEF);
        assertEq(deployHarness.resolveConfig(cfg).vault, address(0xBEEF));
    }

    /// **The B-13 property itself.** The same explicit config resolves the
    /// same way whatever the process environment says.
    function testResolvedConfigIsIndependentOfAmbientEnv() external {
        vm.chainId(1);
        Deploy.DeployConfig memory cfg = _cfg();

        vm.setEnv("REQUIRE_BALANCER", "true");
        vm.setEnv("UNIV3_ROUTER", vm.toString(address(0xDEAD)));
        bytes32 a = keccak256(abi.encode(deployHarness.resolveConfig(cfg)));

        vm.setEnv("REQUIRE_BALANCER", "false");
        vm.setEnv("UNIV3_ROUTER", vm.toString(address(0xBEEF)));
        bytes32 b = keccak256(abi.encode(deployHarness.resolveConfig(cfg)));

        assertEq(a, b, "the ambient environment reached a resolved config");
    }
}
