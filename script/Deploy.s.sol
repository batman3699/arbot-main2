// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import "forge-std/Script.sol";
import "forge-std/console2.sol";

import {MultiVenueArbImplementation} from "../contracts/executor/MultiVenueArbImplementation.sol";
import {ArbitrageCloneFactory} from "../contracts/executor/ArbitrageCloneFactory.sol";
import {BatchRouter} from "../contracts/executor/BatchRouter.sol";

contract Deploy is Script {

    address internal constant DEFAULT_FOUNDRY_SENDER = 0x1804c8AB1F12E6bbf3894d4083f33e07309d1f38;
    error OwnerNotConfigured();
    error MissingRequiredIntegration(string integration, uint256 chainId, string prefix);

    function run()
        external
        returns (MultiVenueArbImplementation impl, ArbitrageCloneFactory factory, address clone, BatchRouter router)
    {
        return _run(bytes32(0));
    }

    function _run(bytes32 salt)
        internal
        returns (MultiVenueArbImplementation impl, ArbitrageCloneFactory factory, address clone, BatchRouter router)
    {
        string memory prefix = _resolveEnvPrefix();

        address vault = _envAddressWithPrefixFallback(prefix, "BAL_VAULT", "BALANCER_VAULT", address(0));
        if (vault == address(0)) {
            vault = _defaultBalancerVault(block.chainid);
        }
        address uniRouter = _envAddressWithPrefix(prefix, "UNIV3_ROUTER", address(0));
        if (uniRouter == address(0)) {
            uniRouter = _envAddressWithPrefix(prefix, "SWAPROUTER02", address(0));
        }
        if (uniRouter == address(0)) {
            uniRouter = _defaultUniV3Router(block.chainid);
        }
        address aavePool = _envAddressWithPrefix(prefix, "AAVE_POOL", address(0));
        if (aavePool == address(0)) {
            aavePool = _defaultAavePool(block.chainid);
        }
        address permit2 = _envAddressWithPrefixFallback(prefix, "PERMIT2_ADDRESS", "PERMIT2", address(0));
        if (permit2 == address(0)) {
            permit2 = _defaultPermit2(block.chainid);
        }
        address owner;

        uint256 privateKey = _envUintOr("PRIVATE_KEY", uint256(0));
        address configuredOwner = _envAddressWithPrefix(prefix, "EXECUTOR_OWNER", address(0));

        if (privateKey != 0) {
            vm.startBroadcast(privateKey);
            owner = vm.addr(privateKey);
        } else {
            vm.startBroadcast();
        }

        if (configuredOwner != address(0)) {
            owner = configuredOwner;
        } else if (owner == address(0)) {
            owner = tx.origin;
        }

        if (owner == address(0)) revert OwnerNotConfigured();
        bool ownerIsDefault = owner == DEFAULT_FOUNDRY_SENDER;
        if (ownerIsDefault && block.chainid != 31337) {
            revert OwnerNotConfigured();
        }

        _validateResolvedIntegrations(prefix, vault, uniRouter, aavePool, permit2);

        bytes32 chosenSalt = salt;
        if (chosenSalt == bytes32(0)) {
            chosenSalt = _envBytes32Or("EXECUTOR_SALT", bytes32(0));
        }
        if (chosenSalt == bytes32(0)) {
            chosenSalt = _envBytes32Or("CREATE2_SALT", bytes32(0));
        }
        if (chosenSalt == bytes32(0)) {
            chosenSalt = keccak256(abi.encodePacked("arb-exec", block.chainid));
        }

        impl = new MultiVenueArbImplementation();
        factory = new ArbitrageCloneFactory(address(impl));
        clone = factory.deployClone(chosenSalt);

        router = new BatchRouter(clone);

        MultiVenueArbImplementation(clone).initialise({
            _owner: address(router),
            _vault: vault,
            _uni: uniRouter,
            _aavePool: aavePool,
            _permit2: permit2,
            feeBps: 300,
            maxSlippageBps: 150,
            deadlineBuffer: 60
        });

        if (owner != msg.sender) {
            router.transferOwnership(owner);
        }

        console2.log("MultiVenue executor clone deployed", clone);
        console2.log("Batch router deployed", address(router));
        console2.log("Env prefix", prefix);
        console2.log("Vault", vault);
        console2.log("Univ3 router", uniRouter);
        console2.log("Aave pool", aavePool);
        console2.log("Permit2", permit2);

        vm.stopBroadcast();
    }

    function _envUintOr(string memory key, uint256 defaultValue) internal view returns (uint256) {
        string memory raw = vm.envOr(key, string(""));
        if (bytes(raw).length == 0) {
            return defaultValue;
        }

        return vm.parseUint(raw);
    }

    function _envAddressOr(string memory key, address defaultValue) internal view returns (address) {
        if (vm.envExists(key)) {
            return vm.envAddress(key);
        }

        return defaultValue;
    }

    function _envAddressWithPrefix(
        string memory prefix,
        string memory key,
        address defaultValue
    ) internal view returns (address) {
        if (bytes(prefix).length == 0) {
            return _envAddressOr(key, defaultValue);
        }
        address prefixedValue = _envAddressByPrefixedKey(prefix, key);
        if (prefixedValue != address(0)) {
            return prefixedValue;
        }
        return _envAddressOr(key, defaultValue);
    }

    function _envAddressWithPrefixFallback(
        string memory prefix,
        string memory canonicalKey,
        string memory legacyKey,
        address defaultValue
    ) internal view returns (address) {
        if (bytes(prefix).length != 0) {
            address canonicalValue = _envAddressByPrefixedKey(prefix, canonicalKey);
            if (canonicalValue != address(0)) return canonicalValue;

            address legacyValue = _envAddressByPrefixedKey(prefix, legacyKey);
            if (legacyValue != address(0)) return legacyValue;
        }

        if (vm.envExists(canonicalKey)) {
            return vm.envAddress(canonicalKey);
        }
        if (vm.envExists(legacyKey)) {
            return vm.envAddress(legacyKey);
        }

        return defaultValue;
    }

    function _envAddressByPrefixedKey(string memory prefix, string memory key) internal view returns (address) {
        string memory prefixed = string.concat(prefix, "_", key);
        if (vm.envExists(prefixed)) {
            return vm.envAddress(prefixed);
        }

        string memory aliasPrefix = _prefixAlias(prefix);
        if (bytes(aliasPrefix).length == 0) {
            return address(0);
        }

        string memory aliasPrefixed = string.concat(aliasPrefix, "_", key);
        if (vm.envExists(aliasPrefixed)) {
            return vm.envAddress(aliasPrefixed);
        }

        return address(0);
    }

    function _prefixAlias(string memory prefix) internal pure returns (string memory) {
        bytes32 normalizedPrefix = keccak256(bytes(_toEnvPrefix(prefix)));

        if (normalizedPrefix == keccak256(bytes("ETH"))) return "ETHEREUM";
        if (normalizedPrefix == keccak256(bytes("ETHEREUM"))) return "ETH";
        if (normalizedPrefix == keccak256(bytes("ARB"))) return "ARBITRUM";
        if (normalizedPrefix == keccak256(bytes("ARBITRUM"))) return "ARB";
        if (normalizedPrefix == keccak256(bytes("OPT"))) return "OPTIMISM";
        if (normalizedPrefix == keccak256(bytes("OPTIMISM"))) return "OPT";

        return "";
    }

    function _envStringOr(string memory key, string memory defaultValue) internal view returns (string memory) {
        if (vm.envExists(key)) {
            return vm.envString(key);
        }
        return defaultValue;
    }

    function _envBytes32Or(string memory key, bytes32 defaultValue) internal view returns (bytes32) {
        if (vm.envExists(key)) {
            return vm.envBytes32(key);
        }
        return defaultValue;
    }

    function _toEnvPrefix(string memory raw) internal pure returns (string memory) {
        bytes memory input = bytes(raw);
        if (input.length == 0) {
            return "";
        }
        bytes memory output = new bytes(input.length);
        for (uint256 i = 0; i < input.length; i++) {
            bytes1 c = input[i];
            if (c >= 0x61 && c <= 0x7A) {
                output[i] = bytes1(uint8(c) - 32);
            } else if (c == 0x2D || c == 0x20) {
                output[i] = "_";
            } else {
                output[i] = c;
            }
        }
        return string(output);
    }

    function _resolveEnvPrefix() internal view returns (string memory) {
        string memory configuredPrefix = _envStringOr("ENV_PREFIX", "");
        if (bytes(configuredPrefix).length != 0) {
            return _toEnvPrefix(configuredPrefix);
        }

        string memory chainName = _envStringOr("CHAIN", "");
        string memory mappedPrefix = _mappedChainEnvPrefix(chainName);
        if (bytes(mappedPrefix).length != 0) {
            return mappedPrefix;
        }

        return _toEnvPrefix(chainName);
    }

    function _mappedChainEnvPrefix(string memory chainName) internal pure returns (string memory) {
        bytes32 normalizedChain = keccak256(bytes(_toEnvPrefix(chainName)));

        if (normalizedChain == keccak256(bytes("ETHEREUM")) || normalizedChain == keccak256(bytes("ETH"))) {
            return "ETH";
        }
        if (normalizedChain == keccak256(bytes("ARBITRUM")) || normalizedChain == keccak256(bytes("ARB"))) {
            return "ARB";
        }
        if (normalizedChain == keccak256(bytes("OPTIMISM")) || normalizedChain == keccak256(bytes("OPT"))) {
            return "OPT";
        }
        if (normalizedChain == keccak256(bytes("BASE"))) {
            return "BASE";
        }
        if (normalizedChain == keccak256(bytes("POLYGON"))) {
            return "POLYGON";
        }
        if (normalizedChain == keccak256(bytes("ABSTRACT"))) {
            return "ABSTRACT";
        }
        if (normalizedChain == keccak256(bytes("INK"))) {
            return "INK";
        }
        if (normalizedChain == keccak256(bytes("LINEA"))) {
            return "LINEA";
        }

        return "";
    }

    function _validateResolvedIntegrations(
        string memory prefix,
        address vault,
        address uniRouter,
        address aavePool,
        address permit2
    ) internal view {
        if (_isUniV3Required(block.chainid) && uniRouter == address(0)) {
            revert MissingRequiredIntegration("UNIV3_ROUTER|SWAPROUTER02", block.chainid, prefix);
        }
        if (_isPermit2Required(block.chainid) && permit2 == address(0)) {
            revert MissingRequiredIntegration("PERMIT2_ADDRESS|PERMIT2", block.chainid, prefix);
        }
        if (_isBalancerRequired(prefix) && vault == address(0)) {
            revert MissingRequiredIntegration("BAL_VAULT|BALANCER_VAULT", block.chainid, prefix);
        }
        if (_isAaveRequired(prefix) && aavePool == address(0)) {
            revert MissingRequiredIntegration("AAVE_POOL", block.chainid, prefix);
        }
    }

    function _isUniV3Required(uint256 chainId) internal pure returns (bool) {
        return chainId != 31337;
    }

    function _isPermit2Required(uint256 chainId) internal pure returns (bool) {
        return chainId != 31337;
    }

    function _isBalancerRequired(string memory prefix) internal view returns (bool) {
        return _envBoolWithPrefix(prefix, "REQUIRE_BALANCER", false);
    }

    function _isAaveRequired(string memory prefix) internal view returns (bool) {
        return _envBoolWithPrefix(prefix, "REQUIRE_AAVE", false);
    }

    function _envBoolWithPrefix(string memory prefix, string memory key, bool defaultValue) internal view returns (bool) {
        if (bytes(prefix).length != 0) {
            string memory prefixed = string.concat(prefix, "_", key);
            if (vm.envExists(prefixed)) {
                return vm.envBool(prefixed);
            }

            string memory aliasPrefix = _prefixAlias(prefix);
            if (bytes(aliasPrefix).length != 0) {
                string memory aliasPrefixed = string.concat(aliasPrefix, "_", key);
                if (vm.envExists(aliasPrefixed)) {
                    return vm.envBool(aliasPrefixed);
                }
            }
        }

        if (vm.envExists(key)) {
            return vm.envBool(key);
        }

        return defaultValue;
    }

    function _defaultBalancerVault(uint256 chainId) internal pure returns (address) {
        if (chainId == 1) {
            return 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
        }

        return address(0);
    }

    function _defaultAavePool(uint256 chainId) internal pure returns (address) {
        if (chainId == 1) {
            return 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;
        }

        return address(0);
    }

    function _defaultPermit2(uint256 chainId) internal pure returns (address) {
        if (chainId == 1) {
            return 0x000000000022D473030F116dDEE9F6B43aC78BA3;
        }

        return address(0);
    }

    function _defaultUniV3Router(uint256 chainId) internal pure returns (address) {
        if (chainId == 1) {
            return 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45;
        }

        return address(0);
    }
}
