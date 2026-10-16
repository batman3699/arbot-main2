// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {LeanExecutor} from "../../contracts/lean/LeanExecutor.sol";
import {BadPlan} from "../../contracts/lean/LeanPlanCodec.sol";
import {LeanToken, MockV3Pool, MockAeroPool, MockVault, Poker} from "./LeanMocks.sol";
import {MockMorphoFlash} from "../../contracts/mocks/MockMorphoFlash.sol";

/// Task 8.5 R29: the lean executor's run path, against venue doubles. A trade
/// borrows WETH from Balancer, swaps hop by hop paying each pool in its own
/// callback, clears the floor, pays the profit to the recipient and repays.
contract LeanExecutorRunTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    address constant MORPHO = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;

    struct Hop {
        uint8 kind;
        address pool;
        address tokenOut;
        uint128 minOut;
    }

    event Profit(uint256 gross);

    address owner = address(0xA11CE);
    address trader = address(0xB0B);
    address recipient = address(0xCAFE);
    LeanToken weth;
    LeanToken x;
    MockV3Pool wethToX;
    MockV3Pool xToWeth;
    LeanExecutor lean;

    uint128 constant LOAN = 1e18;
    uint256 constant X_OUT = 2000e18;
    /// 2000e18 X at 1/1990: the cycle's WETH out.
    uint256 constant WETH_OUT = uint256(2000e18) / 1990;

    function setUp() public {
        vm.etch(WETH, address(new LeanToken()).code);
        weth = LeanToken(WETH);
        x = new LeanToken();
        vm.etch(VAULT, address(new MockVault()).code);
        weth.mint(VAULT, 1000e18);
        wethToX = new MockV3Pool(WETH, address(x), 2000, 1, false);
        xToWeth = new MockV3Pool(address(x), WETH, 1, 1990, true);
        x.mint(address(wethToX), 1e30);
        weth.mint(address(xToWeth), 1000e18);
        lean = new LeanExecutor(owner, trader, recipient);
    }

    function plan(uint8 lender, uint128 amount, uint40 deadline, uint128 minProfit, Hop[] memory hops)
        internal
        pure
        returns (bytes memory p)
    {
        p = abi.encodePacked(lender, amount, deadline, minProfit);
        for (uint256 i; i < hops.length; i++) {
            p = abi.encodePacked(p, hops[i].kind, hops[i].pool, hops[i].tokenOut, hops[i].minOut);
        }
    }

    function cycle(uint128 lastMin, uint128 minProfit) internal view returns (bytes memory) {
        Hop[] memory hops = new Hop[](2);
        hops[0] = Hop(0, address(wethToX), address(x), uint128(X_OUT));
        hops[1] = Hop(1, address(xToWeth), WETH, lastMin);
        return plan(0, LOAN, uint40(block.timestamp + 60), minProfit, hops);
    }

    function testTwoHopsPayTheProfitAndRepay() public {
        vm.expectEmit(address(lean));
        emit Profit(WETH_OUT - LOAN);
        vm.prank(trader);
        lean.run(cycle(LOAN + 1, 1));
        assertEq(weth.balanceOf(recipient), WETH_OUT - LOAN, "the gross, to the recipient");
        assertEq(weth.balanceOf(VAULT), 1000e18, "the loan, back");
        assertEq(weth.balanceOf(address(lean)), 0);
        assertEq(x.balanceOf(address(lean)), 0, "nothing left in the middle");
    }

    function testAerodromeV2Hop() public {
        MockAeroPool aero = new MockAeroPool(address(x), WETH, 1, 1990);
        weth.mint(address(aero), 1000e18);
        uint128 asked = uint128(WETH_OUT - 7);
        Hop[] memory hops = new Hop[](2);
        hops[0] = Hop(0, address(wethToX), address(x), uint128(X_OUT));
        hops[1] = Hop(2, address(aero), WETH, asked);
        vm.expectEmit(address(lean));
        emit Profit(asked - LOAN);
        vm.prank(trader);
        lean.run(plan(0, LOAN, uint40(block.timestamp + 60), 1, hops));
        assertEq(weth.balanceOf(recipient), asked - LOAN);
        assertEq(weth.balanceOf(VAULT), 1000e18);
    }

    function testEachHopSpendsWhatTheLastReturned() public {
        vm.prank(trader);
        lean.run(cycle(LOAN + 1, 1));
        assertEq(wethToX.lastAmountSpecified(), int256(uint256(LOAN)), "hop 0 spends the loan");
        assertEq(xToWeth.lastAmountSpecified(), int256(X_OUT), "hop 1 spends hop 0's output");
    }

    function testHopBelowItsMinimumReverts() public {
        Hop[] memory hops = new Hop[](2);
        hops[0] = Hop(0, address(wethToX), address(x), uint128(X_OUT + 1));
        hops[1] = Hop(1, address(xToWeth), WETH, LOAN + 1);
        vm.expectRevert(abi.encodeWithSelector(LeanExecutor.HopShort.selector, 0, X_OUT, X_OUT + 1));
        vm.prank(trader);
        lean.run(plan(0, LOAN, uint40(block.timestamp + 60), 1, hops));
    }

    function testFloorReverts() public {
        uint128 minProfit = 1e17;
        vm.expectRevert(abi.encodeWithSelector(LeanExecutor.Floor.selector, WETH_OUT, LOAN + minProfit));
        vm.prank(trader);
        lean.run(cycle(1, minProfit));
    }

    function testTheFloorCountsTheLendersFee() public {
        MockVault(VAULT).setFee(WETH_OUT - LOAN);
        vm.expectRevert(abi.encodeWithSelector(LeanExecutor.Floor.selector, WETH_OUT, WETH_OUT + 1));
        vm.prank(trader);
        lean.run(cycle(LOAN + 1, 1));
    }

    function testNotTraderReverts() public {
        bytes memory p = cycle(LOAN + 1, 1);
        vm.expectRevert(LeanExecutor.NotTrader.selector);
        vm.prank(owner);
        lean.run(p);
    }

    function testCircuitOpenReverts() public {
        vm.prank(owner);
        lean.tripCircuit();
        bytes memory p = cycle(LOAN + 1, 1);
        vm.expectRevert(LeanExecutor.CircuitOpen.selector);
        vm.prank(trader);
        lean.run(p);
    }

    function testExpiredRevertsBeforeAnyLoan() public {
        bytes memory p = cycle(LOAN + 1, 1);
        uint256 deadline = block.timestamp + 60;
        vm.warp(deadline + 1);
        vm.expectCall(VAULT, abi.encodeWithSelector(MockVault.flashLoan.selector), 0);
        vm.expectRevert(abi.encodeWithSelector(LeanExecutor.Expired.selector, deadline, deadline + 1));
        vm.prank(trader);
        lean.run(p);
    }

    /// Morpho Blue lends, calls back, and pulls its repayment: no fee.
    function testMorphoLoanPaysTheProfitAndIsRepaid() public {
        vm.etch(MORPHO, address(new MockMorphoFlash()).code);
        weth.mint(MORPHO, 1000e18);
        Hop[] memory hops = new Hop[](2);
        hops[0] = Hop(0, address(wethToX), address(x), uint128(X_OUT));
        hops[1] = Hop(1, address(xToWeth), WETH, LOAN + 1);
        vm.expectCall(VAULT, abi.encodeWithSelector(MockVault.flashLoan.selector), 0);
        vm.expectEmit(address(lean));
        emit Profit(WETH_OUT - LOAN);
        vm.prank(trader);
        lean.run(plan(1, LOAN, uint40(block.timestamp + 60), 1, hops));
        assertEq(weth.balanceOf(recipient), WETH_OUT - LOAN);
        assertEq(weth.balanceOf(MORPHO), 1000e18, "Morpho pulled its loan back");
        assertEq(weth.balanceOf(address(lean)), 0);
    }

    function testMorphoCallbackFromOutsideATradeReverts() public {
        bytes memory p = cycle(LOAN + 1, 1);
        vm.expectRevert(LeanExecutor.UnexpectedCallback.selector);
        lean.onMorphoFlashLoan(LOAN, p);
        vm.expectRevert(LeanExecutor.UnexpectedCallback.selector);
        vm.prank(MORPHO);
        lean.onMorphoFlashLoan(LOAN, p);
    }

    /// Review Focus 1: anyone can ask Balancer for a loan naming the executor.
    function testAFlashLoanSomeoneElseStartsReverts() public {
        address[] memory tokens = new address[](1);
        tokens[0] = WETH;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = LOAN;
        bytes memory p = cycle(LOAN + 1, 1);
        vm.expectRevert(LeanExecutor.UnexpectedCallback.selector);
        MockVault(VAULT).flashLoan(address(lean), tokens, amounts, p);
    }

    /// Review Focus 2: a callback pays only the pool being swapped right now.
    function testACallbackFromOutsideATradeReverts() public {
        x.mint(address(lean), 5);
        vm.expectRevert(LeanExecutor.UnexpectedCallback.selector);
        lean.uniswapV3SwapCallback(5, 0, "");
        vm.expectRevert(LeanExecutor.UnexpectedCallback.selector);
        lean.pancakeV3SwapCallback(0, 5, "");
        assertEq(x.balanceOf(address(lean)), 5);
    }

    function testACallbackFromTheWrongPoolReverts() public {
        wethToX.setDelegate(new Poker());
        bytes memory p = cycle(LOAN + 1, 1);
        vm.expectRevert(LeanExecutor.UnexpectedCallback.selector);
        vm.prank(trader);
        lean.run(p);
    }

    function testASecondCallbackReverts() public {
        wethToX.setCallTwice(true);
        bytes memory p = cycle(LOAN + 1, 1);
        vm.expectRevert(LeanExecutor.UnexpectedCallback.selector);
        vm.prank(trader);
        lean.run(p);
    }

    /// Review Focus 3: a plan the contract cannot read exactly takes no loan.
    function testMalformedPlansRevertBeforeAnyLoan() public {
        bytes memory good = cycle(LOAN + 1, 1);
        bytes[] memory bad = new bytes[](6);
        bad[0] = slice(good, 94);
        bad[1] = abi.encodePacked(slice(good, 95), bytes1(0));
        bad[2] = slice(good, 38);
        Hop[] memory hops = new Hop[](2);
        hops[0] = Hop(0, address(wethToX), address(x), uint128(X_OUT));
        hops[1] = Hop(1, address(xToWeth), address(x), LOAN + 1);
        bad[3] = plan(0, LOAN, uint40(block.timestamp + 60), 1, hops);
        hops[1] = Hop(3, address(xToWeth), WETH, LOAN + 1);
        bad[4] = plan(0, LOAN, uint40(block.timestamp + 60), 1, hops);
        hops[1] = Hop(1, address(xToWeth), WETH, LOAN + 1);
        bad[5] = plan(2, LOAN, uint40(block.timestamp + 60), 1, hops);
        // No loan for any of them: counted once, over the whole loop.
        vm.expectCall(VAULT, abi.encodeWithSelector(MockVault.flashLoan.selector), 0);
        for (uint256 i; i < bad.length; i++) {
            vm.expectRevert(BadPlan.selector);
            vm.prank(trader);
            lean.run(bad[i]);
        }
    }

    /// Review Focus 4: a token that delivers less than the pool reports. The
    /// next pool, checking it was paid in full as v3-core does, refuses the
    /// payment, so the trade reverts whether or not the executor holds dust.
    function testAShortDeliveringTokenCannotDrawOnTheLoan() public {
        x.setShort(true);
        bytes memory p = cycle(LOAN + 1, 1);
        vm.expectRevert();
        vm.prank(trader);
        lean.run(p);

        x.mint(address(lean), 5);
        vm.expectRevert();
        vm.prank(trader);
        lean.run(p);
        assertEq(x.balanceOf(address(lean)), 5, "the dust is untouched");
        assertEq(weth.balanceOf(VAULT), 1000e18);
    }

    function slice(bytes memory b, uint256 n) internal pure returns (bytes memory out) {
        out = new bytes(n);
        for (uint256 i; i < n; i++) {
            out[i] = b[i];
        }
    }
}
