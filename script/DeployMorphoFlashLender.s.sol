// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Script, console2} from "forge-std/Script.sol";
import {MorphoFlashLender} from "../contracts/lenders/MorphoFlashLender.sol";

/// R27: deploy MorphoFlashLender on Base. No constructor arguments, no owner,
/// nothing to configure afterwards.
///
///   forge script script/DeployMorphoFlashLender.s.sol --rpc-url base                 # dry run
///   forge script script/DeployMorphoFlashLender.s.sol --rpc-url base --broadcast …   # the operator
contract DeployMorphoFlashLender is Script {
    function run() external returns (MorphoFlashLender lender) {
        require(block.chainid == 8453, "Base only");
        vm.startBroadcast();
        lender = new MorphoFlashLender();
        vm.stopBroadcast();
        // Forge simulates the whole script before it broadcasts anything, so a
        // failed check here stops the deploy as well as the dry run.
        require(lender.MORPHO().code.length > 0, "no Morpho Blue at the constant");
        console2.log("MorphoFlashLender", address(lender));
        console2.log("runtime code hash");
        console2.logBytes32(keccak256(address(lender).code));
    }
}
