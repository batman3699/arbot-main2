// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {FundArmedSenders} from "../script/FundArmedSenders.s.sol";
import {AuthorizeArmedSenders} from "../script/AuthorizeArmedSenders.s.sol";
import {LeanExecutor3} from "../contracts/lean/LeanExecutor3.sol";
import {MockERC20} from "../contracts/mocks/MockERC20.sol";

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

/// R40.1: the batch path sets every sender in one `setExecutors` call on
/// `LeanExecutor3`, at the example file's executor address.
contract AuthorizeArmedSendersBatchTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant EXECUTOR = 0xd25BD9A60113728a2ECb9FdC9392Cde8D906f2c5;
    address constant A = 0x00000000000000000000000000000000000000A1;
    address constant B = 0x00000000000000000000000000000000000000A2;

    function test_one_transaction_authorizes_the_whole_set() public {
        vm.chainId(8453);
        vm.etch(WETH, address(new MockERC20("Wrapped Ether", "WETH", 18)).code);
        vm.etch(USDC, address(new MockERC20("USD Coin", "USDC", 6)).code);
        vm.etch(EXECUTOR, address(new LeanExecutor3(DEFAULT_SENDER, address(0xB0B), DEFAULT_SENDER)).code);
        // The owner, slot 0, as the constructor would have written it there.
        vm.store(EXECUTOR, bytes32(0), bytes32(uint256(uint160(DEFAULT_SENDER))));
        vm.expectCall(EXECUTOR, abi.encodeWithSelector(LeanExecutor3.setExecutors.selector), 1);
        new AuthorizeArmedSenders().runBatchFrom("ops/armed-senders.example.json");
        assertTrue(LeanExecutor3(EXECUTOR).executors(A));
        assertTrue(LeanExecutor3(EXECUTOR).executors(B));
    }
}
