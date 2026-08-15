// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

interface IBridgeAdapter {
    function bridge(bytes calldata data) external returns (bytes32 transferId, uint256 feePaid);
}

library BridgeLib {
    error BridgeTimeout(uint256 elapsed, uint256 maxDuration);
    error BridgeFailed();
    error InvalidBridge();

    struct BridgeParams {
        address bridge;
        uint256 targetChainId;
        bytes callData;
        uint256 maxDuration;
    }

    event BridgeExecuted(
        address indexed bridge, bytes32 indexed transferId, uint256 feePaid, uint256 elapsed, uint256 targetChainId
    );

    function executeBridge(BridgeParams memory params) internal returns (bytes32 transferId, uint256 feePaid) {
        if (params.bridge == address(0)) revert InvalidBridge();
        uint256 start = block.timestamp;
        (transferId, feePaid) = IBridgeAdapter(params.bridge).bridge(params.callData);
        if (transferId == bytes32(0)) revert BridgeFailed();
        uint256 elapsed = block.timestamp - start;
        if (params.maxDuration > 0 && elapsed > params.maxDuration) {
            revert BridgeTimeout(elapsed, params.maxDuration);
        }
        emit BridgeExecuted(params.bridge, transferId, feePaid, elapsed, params.targetChainId);
    }
}
