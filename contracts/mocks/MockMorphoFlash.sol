// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {MockERC20} from "./MockERC20.sol";

interface IMorphoFlashLoanCallback {
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external;
}

/// Morpho Blue's `flashLoan`, as it behaves on Base: send the assets, call the
/// caller back, pull the assets back. Stateless, so `vm.etch` can put it at
/// Morpho's address.
contract MockMorphoFlash {
    function flashLoan(address token, uint256 assets, bytes calldata data) external {
        require(assets != 0, "zero assets");
        MockERC20(token).transfer(msg.sender, assets);
        IMorphoFlashLoanCallback(msg.sender).onMorphoFlashLoan(assets, data);
        MockERC20(token).transferFrom(msg.sender, address(this), assets);
    }
}
