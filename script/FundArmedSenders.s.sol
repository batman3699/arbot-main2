// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Script, console2} from "forge-std/Script.sol";

/// R36: top every armed sender up to `amount_wei` (0.002 ETH in the pilot):
/// what one sender's worst cases and gas spend from. A sender already at or
/// over it is sent nothing, so a second run refills rather than doubles.
/// Reads `ops/armed-senders.json` (see `ops/host/ARMED.md`).
///
///   forge script script/FundArmedSenders.s.sol --rpc-url https://mainnet.base.org --sender <owner>   # dry run
///   (the same, with --broadcast --interactive)                                                        # the owner
///
/// `--sig "run(string)" <path>` reads another file.
contract FundArmedSenders is Script {
    /// A slipped decimal point stops here: no sender holds more.
    uint256 constant MOST = 0.01 ether;

    function run() external {
        _run("ops/armed-senders.json");
    }

    function run(string calldata path) external {
        _run(path);
    }

    function _run(string memory path) private {
        require(block.chainid == 8453, "Base only");
        string memory json = vm.readFile(path);
        address[] memory senders = vm.parseJsonAddressArray(json, ".senders");
        uint256 amount = vm.parseUint(vm.parseJsonString(json, ".amount_wei"));
        require(senders.length > 0, "no senders");
        require(amount > 0 && amount <= MOST, "amount_wei is 1 wei to 0.01 ETH");
        uint256 total;
        vm.startBroadcast();
        for (uint256 i; i < senders.length; ++i) {
            uint256 held = senders[i].balance;
            if (held >= amount) continue;
            total += amount - held;
            (bool ok,) = payable(senders[i]).call{value: amount - held}("");
            require(ok, "a transfer failed");
        }
        vm.stopBroadcast();
        for (uint256 i; i < senders.length; ++i) {
            require(senders[i].balance >= amount, "a sender is short");
            console2.log(senders[i], senders[i].balance);
        }
        console2.log("sent (wei)", total);
    }
}
