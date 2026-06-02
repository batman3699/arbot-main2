// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

interface ILendingAdapter {
    function health(address borrower) external view returns (uint256);
    function liquidate(bytes calldata data) external returns (uint256 seizedCollateral);
}

library LiquidationLib {
    error PositionHealthy(uint256 healthFactor);
    error InvalidLendingAdapter();

    event LiquidationExecuted(address indexed adapter, address indexed borrower, uint256 seizedCollateral);

    uint256 internal constant HEALTH_THRESHOLD = 1e18;

    struct LiquidationParams {
        address adapter;
        address borrower;
        bytes callData;
    }

    function executeLiquidation(LiquidationParams memory params) internal returns (uint256 seizedCollateral) {
        if (params.adapter == address(0)) revert InvalidLendingAdapter();
        uint256 healthFactor = ILendingAdapter(params.adapter).health(params.borrower);
        if (healthFactor >= HEALTH_THRESHOLD) {
            revert PositionHealthy(healthFactor);
        }
        seizedCollateral = ILendingAdapter(params.adapter).liquidate(params.callData);
        emit LiquidationExecuted(params.adapter, params.borrower, seizedCollateral);
    }
}
