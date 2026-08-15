// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {IDexAdapter} from "../libraries/DexLib.sol";
import {MockERC20} from "./MockERC20.sol";

contract MockDex is IDexAdapter {
    function executeSwap(bytes calldata data) external override returns (uint256 amountOut) {
        (address tokenIn, address tokenOut, uint256 amountIn, uint256 outAmount) =
            abi.decode(data, (address, address, uint256, uint256));
        require(MockERC20(tokenIn).transferFrom(msg.sender, address(this), amountIn), "in transfer failed");
        require(MockERC20(tokenOut).transfer(msg.sender, outAmount), "out transfer failed");
        return outAmount;
    }
}
