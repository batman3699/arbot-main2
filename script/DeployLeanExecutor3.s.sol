// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Script, console2} from "forge-std/Script.sol";
import {LeanExecutor3} from "../contracts/lean/LeanExecutor3.sol";

interface IAllowance {
    function allowance(address owner, address spender) external view returns (uint256);
}

/// R40.1: deploy the third lean executor on Base, owned by `owner`, trading for
/// `trader`, paying its profit to `owner`. Then authorize the armed senders in
/// one batch (`AuthorizeArmedSenders --sig "runBatch()"`). The logged code hash
/// is the configuration's `armed.sized.executor3.code_hash`.
///
/// `via_ir` bytecode depends on the compile set, so the owner deploys from a
/// clean build of this script alone and checks the printed hash:
///
///   forge clean && forge build script/DeployLeanExecutor3.s.sol
///   forge script script/DeployLeanExecutor3.s.sol --sig "run(address,address)" <owner> <trader> \
///     --rpc-url https://mainnet.base.org --sender <owner>                                   # dry run
///   (the same, with --broadcast --interactive)                                              # the owner
contract DeployLeanExecutor3 is Script {
    function run(address owner, address trader) external returns (LeanExecutor3 lean, bytes32 codeHash) {
        require(block.chainid == 8453, "Base only");
        vm.startBroadcast();
        lean = new LeanExecutor3(owner, trader, owner);
        vm.stopBroadcast();
        // Forge simulates the whole script before it broadcasts anything, so a
        // failed check here stops the deploy as well as the dry run.
        require(lean.owner() == owner && lean.executors(trader) && lean.profitRecipient() == owner, "not configured");
        require(
            IAllowance(lean.USDC()).allowance(address(lean), lean.MORPHO_BLUE()) == type(uint256).max,
            "Morpho cannot pull USDC"
        );
        codeHash = keccak256(address(lean).code);
        console2.log("LeanExecutor3", address(lean));
        console2.log("runtime code hash (armed.sized.executor3.code_hash)");
        console2.logBytes32(codeHash);
    }
}
