// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import "forge-std/Test.sol";
import "../script/Deploy.s.sol";

contract DeployHarness is Deploy {
    function resolveEnvPrefix() external view returns (string memory) {
        return _resolveEnvPrefix();
    }

    function envAddressWithResolvedPrefix(string memory key, address defaultValue) external view returns (address) {
        return _envAddressWithPrefix(_resolveEnvPrefix(), key, defaultValue);
    }

    function envAddressWithResolvedPrefixFallback(
        string memory canonicalKey,
        string memory legacyKey,
        address defaultValue
    ) external view returns (address) {
        return _envAddressWithPrefixFallback(_resolveEnvPrefix(), canonicalKey, legacyKey, defaultValue);
    }

    function defaultUniV3Router(uint256 chainId) external pure returns (address) {
        return _defaultUniV3Router(chainId);
    }

    function defaultPermit2(uint256 chainId) external pure returns (address) {
        return _defaultPermit2(chainId);
    }

    function defaultBalancerVault(uint256 chainId) external pure returns (address) {
        return _defaultBalancerVault(chainId);
    }

    function defaultAavePool(uint256 chainId) external pure returns (address) {
        return _defaultAavePool(chainId);
    }
}

contract DeployEnvPrefixTest is Test {
    DeployHarness internal harness;

    function setUp() external {
        harness = new DeployHarness();
    }

    function testResolveEnvPrefixMapsOptimismToOpt() external {
        vm.setEnv("CHAIN", "optimism");

        string memory prefix = harness.resolveEnvPrefix();

        assertEq(prefix, "OPT");
    }

    function testEnvAddressLookupUsesMappedOptimismPrefix() external {
        vm.setEnv("CHAIN", "optimism");
        vm.setEnv("OPT_UNIV3_ROUTER", vm.toString(address(0x1001)));
        vm.setEnv("OPTIMISM_UNIV3_ROUTER", vm.toString(address(0x1002)));
        vm.setEnv("UNIV3_ROUTER", vm.toString(address(0x1003)));

        address resolved = harness.envAddressWithResolvedPrefix("UNIV3_ROUTER", address(0));

        assertEq(resolved, address(0x1001));
    }

    function testEnvAddressFallbackLookupUsesMappedOptimismPrefix() external {
        vm.setEnv("CHAIN", "optimism");
        vm.setEnv("OPT_PERMIT2_ADDRESS", vm.toString(address(0x2001)));
        vm.setEnv("OPT_PERMIT2", vm.toString(address(0x2002)));
        vm.setEnv("PERMIT2_ADDRESS", vm.toString(address(0x2003)));

        address resolved = harness.envAddressWithResolvedPrefixFallback("PERMIT2_ADDRESS", "PERMIT2", address(0));

        assertEq(resolved, address(0x2001));
    }

    function testEnvAddressLookupAcceptsEthereumLongPrefixAlias() external {
        vm.setEnv("CHAIN", "eth");
        vm.setEnv("ETHEREUM_UNIV3_ROUTER", vm.toString(address(0x3001)));

        address resolved = harness.envAddressWithResolvedPrefix("UNIV3_ROUTER", address(0));

        assertEq(resolved, address(0x3001));
    }

    function testDefaultBalancerVaultForEthereumMainnet() external view {
        address resolved = harness.defaultBalancerVault(1);

        assertEq(resolved, 0xBA12222222228d8Ba445958a75a0704d566BF2C8);
    }

    function testDefaultAavePoolForEthereumMainnet() external view {
        address resolved = harness.defaultAavePool(1);

        assertEq(resolved, 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2);
    }

    function testDefaultPermit2ForEthereumMainnet() external view {
        address resolved = harness.defaultPermit2(1);

        assertEq(resolved, 0x000000000022D473030F116dDEE9F6B43aC78BA3);
    }

    function testDefaultUniV3RouterForEthereumMainnet() external view {
        address resolved = harness.defaultUniV3Router(1);

        assertEq(resolved, 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45);
    }
}
