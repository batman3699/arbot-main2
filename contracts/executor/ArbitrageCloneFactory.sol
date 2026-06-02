// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {InvalidImplementation, InitFailed} from "./MultiVenueArbImplementation.sol";

contract ArbitrageCloneFactory {
    error Create2DeploymentFailed(bytes32 salt);

    address public immutable implementation;

    event CloneDeployed(address indexed clone, bytes32 indexed salt);

    constructor(address implementation_) {
        if (implementation_ == address(0)) revert InvalidImplementation();
        implementation = implementation_;
    }

    function deployClone(bytes32 salt) public returns (address clone) {
        bytes memory creation = abi.encodePacked(
            hex"3d602d80600a3d3981f3363d3d373d3d3d363d73", implementation, hex"5af43d82803e903d91602b57fd5bf3"
        );

        assembly ("memory-safe") {
            let ptr := add(creation, 0x20)
            let size := mload(creation)
            clone := create2(0, ptr, size, salt)
        }
        if (clone == address(0)) revert Create2DeploymentFailed(salt);
        if (clone == address(0) || clone.code.length == 0) revert InvalidImplementation();
        emit CloneDeployed(clone, salt);
    }

    function deployAndInit(bytes32 salt, bytes calldata initData) external returns (address clone) {
        clone = deployClone(salt);
        (bool ok,) = clone.call(initData);
        if (!ok) revert InitFailed();
    }
}
