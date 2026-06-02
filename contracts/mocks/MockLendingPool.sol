// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {MockERC20} from "./MockERC20.sol";

contract MockLendingPool {
    struct Position {
        uint256 healthFactor;
        uint256 repayAmount;
        uint256 seizeAmount;
        address collateralToken;
    }

    mapping(address => Position) public positions;

    function setPosition(address borrower, Position calldata position) external {
        positions[borrower] = position;
    }

    function health(address borrower) external view returns (uint256) {
        return positions[borrower].healthFactor;
    }

    function liquidate(bytes calldata data) external returns (uint256 seizedCollateral) {
        (address borrower, address debtToken, uint256 repayAmount) = abi.decode(data, (address, address, uint256));
        Position memory position = positions[borrower];
        require(position.repayAmount == repayAmount, "repay mismatch");
        require(MockERC20(debtToken).transferFrom(msg.sender, address(this), repayAmount), "repay transfer failed");
        seizedCollateral = position.seizeAmount;
        require(
            MockERC20(position.collateralToken).transfer(msg.sender, seizedCollateral), "collateral transfer failed"
        );
        positions[borrower].healthFactor = type(uint256).max;
    }
}
