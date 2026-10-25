// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {FundArmedSenders} from "../script/FundArmedSenders.s.sol";

/// R36: the funding script tops each armed sender up to `amount_wei`, and a
/// second run refills rather than doubles. The example file's two senders.
contract FundArmedSendersTest is Test {
    address constant A = 0x00000000000000000000000000000000000000A1;
    address constant B = 0x00000000000000000000000000000000000000A2;
    string constant SENDERS = "ops/armed-senders.example.json";

    function setUp() public {
        vm.chainId(8453);
        vm.deal(DEFAULT_SENDER, 1 ether);
    }

    function test_tops_every_sender_up_to_the_amount_and_no_further() public {
        vm.deal(A, 16_000_000_000_001); // dust already there, as on Base
        FundArmedSenders script = new FundArmedSenders();
        script.runFrom(SENDERS);
        assertEq(A.balance, 2e15);
        assertEq(B.balance, 2e15);
        assertEq(DEFAULT_SENDER.balance, 1 ether - (2e15 - 16_000_000_000_001) - 2e15, "only the shortfall");

        uint256 left = DEFAULT_SENDER.balance;
        script.runFrom(SENDERS);
        assertEq(DEFAULT_SENDER.balance, left, "a second run sends nothing");
    }
}
