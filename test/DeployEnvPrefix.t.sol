// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import "forge-std/Test.sol";
import "../script/Deploy.s.sol";

contract DeployHarness is Deploy {
    function resolveEnvPrefix() external view returns (string memory) {
        return _resolveEnvPrefix();
    }

    /// The prefix is passed in rather than resolved from `CHAIN`.
    ///
    /// That single change is what decouples these tests from each other: the
    /// old harness called `_resolveEnvPrefix()` internally, so every test had
    /// to write the shared `CHAIN` key to steer it, and `CHAIN` persists for
    /// the whole forge process.
    function envAddressWithPrefix(string memory prefix, string memory key, address defaultValue)
        external
        view
        returns (address)
    {
        return _envAddressWithPrefix(prefix, key, defaultValue);
    }

    function envAddressWithPrefixFallback(
        string memory prefix,
        string memory canonicalKey,
        string memory legacyKey,
        address defaultValue
    ) external view returns (address) {
        return _envAddressWithPrefixFallback(prefix, canonicalKey, legacyKey, defaultValue);
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

/// B-13, fixed. These tests are about the env READERS, so they cannot avoid
/// the environment — but they can avoid each other.
///
/// Two rules, and between them the 3-in-8 flake rate goes to zero:
///
/// 1. **Every test uses a key name no other test and no `.env` can define.**
///    The suffix is the test's own name. `.env` supplies real keys like
///    `ETH_UNIV3_ROUTER`, and those correctly outrank the unprefixed ones a
///    test sets — which is why a test that wrote `UNIV3_ROUTER` got whatever
///    `.env` said instead.
/// 2. **The prefix is an argument, not `CHAIN`.** Exactly one test below reads
///    `CHAIN`, because that is the thing it is testing. Every other test names
///    its prefix directly, so nothing writes a key a sibling reads.
///
/// `docs/apex/BASELINE.md` records five approaches that were tried and
/// reverted, including `threads = 1` — confirmed read by `forge config`, and
/// it changed nothing, because the tests share one process either way. There
/// is no `unsetEnv` cheatcode to isolate with. The fix is not isolation; it is
/// not sharing.
contract DeployEnvPrefixTest is Test {
    DeployHarness internal harness;

    function setUp() external {
        harness = new DeployHarness();
    }

    /// The one test that owns `CHAIN`, because prefix resolution is its
    /// subject. Nothing else reads it.
    function testResolveEnvPrefixMapsOptimismToOpt() external {
        vm.setEnv("CHAIN", "optimism");
        assertEq(harness.resolveEnvPrefix(), "OPT");
    }

    function testEnvAddressLookupUsesMappedOptimismPrefix() external {
        vm.setEnv("OPT_ROUTER_MAPPEDOPT", vm.toString(address(0x1001)));
        vm.setEnv("OPTIMISM_ROUTER_MAPPEDOPT", vm.toString(address(0x1002)));
        vm.setEnv("ROUTER_MAPPEDOPT", vm.toString(address(0x1003)));

        assertEq(
            harness.envAddressWithPrefix("OPT", "ROUTER_MAPPEDOPT", address(0)),
            address(0x1001),
            "the exact prefix outranks the alias and the bare key"
        );
    }

    function testEnvAddressFallbackLookupUsesMappedOptimismPrefix() external {
        vm.setEnv("OPT_PERMIT_FALLBACKOPT", vm.toString(address(0x2001)));
        vm.setEnv("OPT_PERMITLEGACY_FALLBACKOPT", vm.toString(address(0x2002)));
        vm.setEnv("PERMIT_FALLBACKOPT", vm.toString(address(0x2003)));

        assertEq(
            harness.envAddressWithPrefixFallback(
                "OPT", "PERMIT_FALLBACKOPT", "PERMITLEGACY_FALLBACKOPT", address(0)
            ),
            address(0x2001),
            "the canonical key wins before the legacy one is consulted"
        );
    }

    /// The long alias is consulted when the short prefix has nothing.
    ///
    /// This is the test that failed most often: it set `CHAIN=eth` and
    /// `ETHEREUM_UNIV3_ROUTER`, and `.env`'s real `ETH_UNIV3_ROUTER` outranked
    /// the alias. With a key `.env` does not define, the precedence it is
    /// actually testing is the only thing in play.
    function testEnvAddressLookupAcceptsEthereumLongPrefixAlias() external {
        vm.setEnv("ETHEREUM_ROUTER_LONGALIAS", vm.toString(address(0x3001)));

        assertEq(
            harness.envAddressWithPrefix("ETH", "ROUTER_LONGALIAS", address(0)),
            address(0x3001)
        );
    }

    /// ...and the short prefix still wins when both exist.
    function testTheShortPrefixOutranksTheLongAlias() external {
        vm.setEnv("ETH_ROUTER_BOTHFORMS", vm.toString(address(0x4001)));
        vm.setEnv("ETHEREUM_ROUTER_BOTHFORMS", vm.toString(address(0x4002)));

        assertEq(
            harness.envAddressWithPrefix("ETH", "ROUTER_BOTHFORMS", address(0)),
            address(0x4001)
        );
    }

    function testDefaultBalancerVaultForEthereumMainnet() external view {
        assertEq(harness.defaultBalancerVault(1), 0xBA12222222228d8Ba445958a75a0704d566BF2C8);
    }

    function testDefaultAavePoolForEthereumMainnet() external view {
        assertEq(harness.defaultAavePool(1), 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2);
    }

    function testDefaultPermit2ForEthereumMainnet() external view {
        assertEq(harness.defaultPermit2(1), 0x000000000022D473030F116dDEE9F6B43aC78BA3);
    }

    function testDefaultUniV3RouterForEthereumMainnet() external view {
        assertEq(harness.defaultUniV3Router(1), 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45);
    }
}
