// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

interface IMultiVenueSwapModule {
    function moduleExecSwap(uint8 op, bytes memory data, uint256 deadline, address recipient, address vaultAddr, address routerAddr) external;
}

contract SwapExecutor {
    function execute(
        uint8 op,
        bytes memory data,
        uint256 deadline,
        address recipient,
        address vaultAddr,
        address routerAddr
    ) external {
        IMultiVenueSwapModule(address(this)).moduleExecSwap(op, data, deadline, recipient, vaultAddr, routerAddr);
    }
}
