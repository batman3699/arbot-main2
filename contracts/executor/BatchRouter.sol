// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {MultiVenueArbImplementation, InvalidBatch, InvalidExecutor, InvalidOwner, InvalidTarget, NotOwner} from "./MultiVenueArbImplementation.sol";

contract BatchRouter {
    address public immutable executor;
    address public owner;

    mapping(address => bool) public isAllowedTarget;

    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);
    event TargetAllowed(address indexed target, bool allowed);

    constructor(address executor_) {
        if (executor_ == address(0)) revert InvalidExecutor();
        executor = executor_;
        owner = msg.sender;
        isAllowedTarget[executor_] = true;
        emit OwnershipTransferred(address(0), msg.sender);
        emit TargetAllowed(executor_, true);
    }

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert InvalidOwner();
        emit OwnershipTransferred(owner, newOwner);
        owner = newOwner;
    }

    function setTargetAllowed(address target, bool allowed) external onlyOwner {
        if (target == address(0)) revert InvalidTarget();
        isAllowedTarget[target] = allowed;
        emit TargetAllowed(target, allowed);
    }

    function start(MultiVenueArbImplementation.PlanLegacy memory plan)
        external
        onlyOwner
        returns (uint256 grossProfit)
    {
        return MultiVenueArbImplementation(executor).start(plan);
    }

    function startV2(MultiVenueArbImplementation.PlanV2 memory plan)
        external
        onlyOwner
        returns (uint256 grossProfit)
    {
        return MultiVenueArbImplementation(executor).startV2(plan);
    }

    function multicall(address[] calldata targets, bytes[] calldata data) external onlyOwner {
        if (targets.length != data.length) revert InvalidBatch();
        for (uint256 i; i < targets.length; ++i) {
            if (!isAllowedTarget[targets[i]]) revert InvalidTarget();
            (bool ok, bytes memory ret) = targets[i].call(data[i]);
            if (!ok) {
                if (ret.length == 0) revert InvalidBatch();
                assembly {
                    revert(add(ret, 0x20), mload(ret))
                }
            }
        }
    }
}

