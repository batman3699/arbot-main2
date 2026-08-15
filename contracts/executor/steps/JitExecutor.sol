// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

interface IMultiVenueJitModule {
    function moduleExecJit(uint8 op, bytes memory data, uint256 deadline) external;
}

contract JitExecutor {
    function execute(uint8 op, bytes memory data, uint256 deadline) external {
        IMultiVenueJitModule(address(this)).moduleExecJit(op, data, deadline);
    }
}
