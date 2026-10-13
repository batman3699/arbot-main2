// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Script, console2} from "forge-std/Script.sol";

interface IWETH9 {
    function deposit() external payable;
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address who) external view returns (uint256);
}

/// R27: leave 1 wei of WETH in the Morpho lender and in the executor, so a
/// Morpho-funded trade never writes either balance from zero.
///
/// A balance written from zero costs ~20k gas more, and the Morpho path's
/// refunds are capped. Measured on ticket 51's cycle at 15 WETH: Morpho's
/// extra gas over Balancer is 121,700 with both balances at zero and 82,900
/// with 1 wei in each. The executor counts profit from its starting balance,
/// and the lender pulls back exactly what it lent, so the wei stays put.
///
/// Sends only to a balance that is zero, so a second run sends nothing.
///
///   forge script script/SeedWethDust.s.sol --rpc-url base                 # dry run
///   forge script script/SeedWethDust.s.sol --rpc-url base --broadcast …   # the owner
contract SeedWethDust is Script {
    IWETH9 constant WETH = IWETH9(0x4200000000000000000000000000000000000006);
    address constant LENDER = 0xDF64CC1991899340edd2e1d870AC7F8c0a4D1A9c;
    address constant EXECUTOR = 0x8940B565D050974b2b589B70De43bCb753b1F93B;
    bytes32 constant LENDER_CODE_HASH = 0x248bf9d05e91ff2e785fe73abbc4f2fb83ed9a533d0d0d93979c19720c6358b4;
    bytes32 constant EXECUTOR_CODE_HASH = 0x68ea95b65ff6d634c1e8e766cf66ecd3996eb0342cdc2f1179485ae03b7fa488;

    function run() external {
        require(block.chainid == 8453, "Base only");
        require(LENDER.codehash == LENDER_CODE_HASH, "not the deployed lender");
        require(EXECUTOR.codehash == EXECUTOR_CODE_HASH, "not the deployed executor");

        address[2] memory targets = [LENDER, EXECUTOR];
        uint256 needed;
        for (uint256 i; i < 2; ++i) {
            if (WETH.balanceOf(targets[i]) == 0) ++needed;
        }
        if (needed == 0) {
            console2.log("both already hold WETH; nothing to send");
            return;
        }

        vm.startBroadcast();
        WETH.deposit{value: needed}();
        for (uint256 i; i < 2; ++i) {
            if (WETH.balanceOf(targets[i]) == 0) require(WETH.transfer(targets[i], 1), "transfer failed");
        }
        vm.stopBroadcast();

        // Forge simulates the whole script before it broadcasts anything, so a
        // failed check here stops the broadcast as well as the dry run.
        require(WETH.balanceOf(LENDER) > 0 && WETH.balanceOf(EXECUTOR) > 0, "not seeded");
        console2.log("lender WETH (wei)", WETH.balanceOf(LENDER));
        console2.log("executor WETH (wei)", WETH.balanceOf(EXECUTOR));
    }
}
