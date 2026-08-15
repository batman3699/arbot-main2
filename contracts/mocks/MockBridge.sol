// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {IBridgeAdapter} from "../libraries/BridgeLib.sol";

contract MockBridge is IBridgeAdapter {
    uint256 public delay;
    uint256 public fee;
    bytes32 public lastTransferId;

    function setDelay(uint256 newDelay) external {
        delay = newDelay;
    }

    function setFee(uint256 newFee) external {
        fee = newFee;
    }

    function bridge(bytes calldata data) external override returns (bytes32 transferId, uint256 feePaid) {
        (bytes32 providedId) = abi.decode(data, (bytes32));
        for (uint256 i = 0; i < delay; i++) {
            // intentional no-op to simulate latency
        }
        transferId = providedId == bytes32(0)
            ? bytes32(uint256(keccak256(abi.encodePacked(block.timestamp, msg.sender))))
            : providedId;
        feePaid = fee;
        lastTransferId = transferId;
    }
}
