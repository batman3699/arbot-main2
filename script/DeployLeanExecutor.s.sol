// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Script, console2} from "forge-std/Script.sol";
import {LeanExecutor} from "../contracts/lean/LeanExecutor.sol";

/// R29: deploy the lean executor on Base, owned by `owner`, trading for
/// `trader`, paying its profit to `owner`. Nothing to configure afterwards; the
/// logged code hash is the configuration's `executor.code_hash`.
///
///   forge script script/DeployLeanExecutor.s.sol --sig "run(address,address)" <owner> <trader> \
///     --rpc-url https://mainnet.base.org --sender <owner>                                   # dry run
///   (the same, with --broadcast --interactive)                                              # the owner
contract DeployLeanExecutor is Script {
    function run(address owner, address trader) external returns (LeanExecutor lean, bytes32 codeHash) {
        require(block.chainid == 8453, "Base only");
        vm.startBroadcast();
        lean = new LeanExecutor(owner, trader, owner);
        vm.stopBroadcast();
        // Forge simulates the whole script before it broadcasts anything, so a
        // failed check here stops the deploy as well as the dry run.
        require(lean.owner() == owner && lean.executors(trader) && lean.profitRecipient() == owner, "not configured");
        codeHash = keccak256(address(lean).code);
        console2.log("LeanExecutor", address(lean));
        console2.log("runtime code hash (executor.code_hash)");
        console2.logBytes32(codeHash);
    }
}
