// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

interface IDexAdapter {
    function executeSwap(bytes calldata data) external returns (uint256 amountOut);
}

library DexLib {
    error SlippageExceeded(uint256 expected, uint256 actual, uint256 maxSlippageBps);
    error InvalidRouter();

    struct SwapParams {
        address router;
        bytes callData;
        uint256 expectedAmountOut;
        uint256 maxSlippageBps;
        uint256 minAmountOut;
    }

    event SwapExecuted(address indexed router, uint256 amountOut, uint256 minAmountOut, uint256 expectedAmountOut);

    uint256 internal constant BPS_DENOMINATOR = 10_000;

    function executeSwap(SwapParams memory params) internal returns (uint256 amountOut) {
        if (params.router == address(0)) revert InvalidRouter();

        amountOut = IDexAdapter(params.router).executeSwap(params.callData);

        if (amountOut < params.minAmountOut) {
            revert SlippageExceeded(params.minAmountOut, amountOut, params.maxSlippageBps);
        }

        if (params.expectedAmountOut > 0 && params.maxSlippageBps > 0) {
            uint256 allowedSlippageBps = BPS_DENOMINATOR - params.maxSlippageBps;
            uint256 actualProduct = amountOut * BPS_DENOMINATOR;
            uint256 expectedProduct = params.expectedAmountOut * allowedSlippageBps;

            if (actualProduct < expectedProduct) {
                revert SlippageExceeded(params.expectedAmountOut, amountOut, params.maxSlippageBps);
            }
        }

        emit SwapExecuted(params.router, amountOut, params.minAmountOut, params.expectedAmountOut);
    }
}
