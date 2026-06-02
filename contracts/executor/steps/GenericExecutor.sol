// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

interface IMultiVenueGenericModule {
    function moduleExecGeneric(bytes memory data) external;
}

contract GenericExecutor {
    function execute(bytes memory data) external {
        IMultiVenueGenericModule(address(this)).moduleExecGeneric(data);
    }
}
