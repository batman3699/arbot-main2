// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {IUniswapV2Pair, IUniswapV3Pool} from "../executor/MultiVenueArbImplementation.sol";
import {MockERC20} from "./MockERC20.sol";

contract MockUniswapV2Pair is IUniswapV2Pair {
    address public immutable override token0;
    address public immutable override token1;

    uint16 public feeBps;

    constructor(address token0_, address token1_, uint16 feeBps_) {
        token0 = token0_;
        token1 = token1_;
        feeBps = feeBps_;
    }

    function setFeeBps(uint16 feeBps_) external {
        feeBps = feeBps_;
    }

    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external override {
        if ((amount0Out == 0) == (amount1Out == 0)) revert("one-sided");

        uint256 amountOut = amount0Out > 0 ? amount0Out : amount1Out;
        address tokenOut = amount0Out > 0 ? token0 : token1;

        uint256 balBefore = MockERC20(tokenOut).balanceOf(address(this));
        MockERC20(tokenOut).transfer(to, amountOut);

        (bool ok,) = to.call(abi.encodeWithSignature("uniswapV2Call(address,uint256,uint256,bytes)", to, amount0Out, amount1Out, data));
        if (!ok) revert("callback");

        uint256 fee = feeBps > 0 ? (amountOut * feeBps) / 10_000 : ((amountOut * 3) / 997) + 1;
        uint256 required = balBefore + fee;
        if (MockERC20(tokenOut).balanceOf(address(this)) < required) revert("underpaid");
    }
}

contract MockUniswapV3FlashPool is IUniswapV3Pool {
    address public immutable override token0;
    address public immutable override token1;

    uint16 public feeBps;

    constructor(address token0_, address token1_, uint16 feeBps_) {
        token0 = token0_;
        token1 = token1_;
        feeBps = feeBps_;
    }

    function fee() external pure override returns (uint24) {
        return 500;
    }

    function tickSpacing() external pure override returns (int24) {
        return 1;
    }

    function slot0() external pure override returns (uint160 sqrtPriceX96, int24 tick, uint16, uint16, uint16, uint8, bool) {
        return (0, 0, 0, 0, 0, 0, false);
    }

    function flash(address recipient, uint256 amount0, uint256 amount1, bytes calldata data) external override {
        if ((amount0 == 0) == (amount1 == 0)) revert("one-sided");

        address token = amount0 > 0 ? token0 : token1;
        uint256 amount = amount0 > 0 ? amount0 : amount1;
        uint256 beforeBal = MockERC20(token).balanceOf(address(this));

        MockERC20(token).transfer(recipient, amount);

        uint256 flashFee = (amount * feeBps) / 10_000;
        (bool ok,) = recipient.call(
            abi.encodeWithSignature("uniswapV3FlashCallback(uint256,uint256,bytes)", amount0 > 0 ? flashFee : 0, amount1 > 0 ? flashFee : 0, data)
        );
        if (!ok) revert("callback");

        if (MockERC20(token).balanceOf(address(this)) < beforeBal + flashFee) revert("underpaid");
    }

    function mint(address, int24, int24, uint128, bytes calldata) external pure override returns (uint256, uint256) {
        revert("unsupported");
    }

    function burn(int24, int24, uint128) external pure override returns (uint256, uint256) {
        revert("unsupported");
    }

    function collect(address, int24, int24, uint128, uint128) external pure override returns (uint256, uint256) {
        revert("unsupported");
    }
}
