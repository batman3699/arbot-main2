// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Script, console2} from "forge-std/Script.sol";
import {LeanExecutor2} from "../contracts/lean/LeanExecutor2.sol";

interface IAllowance {
    function allowance(address owner, address spender) external view returns (uint256);
}

/// R37: deploy the second lean executor on Base, owned by `owner`, trading for
/// `trader`, paying its profit to `owner`. Nothing to configure afterwards
/// beyond authorizing the armed senders (ops/host/ARMED.md, R37); the logged
/// code hash is the configuration's `armed.executor2.code_hash`.
///
///   forge script script/DeployLeanExecutor2.s.sol --sig "run(address,address)" <owner> <trader> \
///     --rpc-url https://mainnet.base.org --sender <owner>                                   # dry run
///   (the same, with --broadcast --interactive)                                              # the owner
contract DeployLeanExecutor2 is Script {
    function run(address owner, address trader) external returns (LeanExecutor2 lean, bytes32 codeHash) {
        require(block.chainid == 8453, "Base only");
        vm.startBroadcast();
        lean = new LeanExecutor2(owner, trader, owner);
        vm.stopBroadcast();
        // Forge simulates the whole script before it broadcasts anything, so a
        // failed check here stops the deploy as well as the dry run.
        require(lean.owner() == owner && lean.executors(trader) && lean.profitRecipient() == owner, "not configured");
        require(
            IAllowance(lean.USDC()).allowance(address(lean), lean.MORPHO_BLUE()) == type(uint256).max,
            "Morpho cannot pull USDC"
        );
        codeHash = keccak256(address(lean).code);
        console2.log("LeanExecutor2", address(lean));
        console2.log("runtime code hash (armed.executor2.code_hash)");
        console2.logBytes32(codeHash);
    }
}
