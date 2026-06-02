// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

interface IMultiVenueBridgeModule {
    function moduleExecBridge(bytes memory data) external;
}

contract BridgeExecutor {
    function execute(bytes memory data, address) external {
        IMultiVenueBridgeModule(address(this)).moduleExecBridge(data);
    }
}
