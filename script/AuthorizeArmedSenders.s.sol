// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Script, console2} from "forge-std/Script.sol";

interface ILeanExecutorA {
    function owner() external view returns (address);
    function executors(address account) external view returns (bool);
    function setExecutor(address account, bool allowed) external;
}

/// R36: let every armed sender trade through the lean executor or, with
/// `"allowed": false`, stop them all. Reads `ops/armed-senders.json`, which
/// the host writes after `apex keys new` (addresses only; see
/// `ops/armed-senders.example.json` and `ops/host/ARMED.md`).
///
///   forge script script/AuthorizeArmedSenders.s.sol --rpc-url https://mainnet.base.org --sender <owner>   # dry run
///   (the same, with --broadcast --interactive)                                                             # the owner
///
/// `--sig "runFrom(string)" <path>` reads another file. (One name each:
/// `forge script` refuses an overloaded `run`.)
contract AuthorizeArmedSenders is Script {
    function run() external {
        _run("ops/armed-senders.json");
    }

    function runFrom(string calldata path) external {
        _run(path);
    }

    function _run(string memory path) private {
        require(block.chainid == 8453, "Base only");
        string memory json = vm.readFile(path);
        ILeanExecutorA executor = ILeanExecutorA(vm.parseJsonAddress(json, ".executor"));
        address[] memory senders = vm.parseJsonAddressArray(json, ".senders");
        bool allowed = vm.parseJsonBool(json, ".allowed");
        require(senders.length > 0, "no senders");
        console2.log("executor", address(executor), "owner", executor.owner());
        vm.startBroadcast();
        for (uint256 i; i < senders.length; ++i) {
            executor.setExecutor(senders[i], allowed);
        }
        vm.stopBroadcast();
        for (uint256 i; i < senders.length; ++i) {
            require(executor.executors(senders[i]) == allowed, "a sender did not take");
            console2.log(senders[i], allowed);
        }
    }
}
