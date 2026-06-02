// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {MockERC20} from "./MockERC20.sol";

contract MockUSDT is MockERC20 {
    error NonZeroAllowance();

    constructor() MockERC20("Tether USD", "USDT", 6) {}

    function approve(address spender, uint256 amount) public override returns (bool) {
        uint256 current = allowance[msg.sender][spender];
        if (amount != 0 && current != 0) {
            revert NonZeroAllowance();
        }
        return super.approve(spender, amount);
    }
}
