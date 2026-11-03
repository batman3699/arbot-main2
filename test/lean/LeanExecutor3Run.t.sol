// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test, Vm} from "forge-std/Test.sol";
import {LeanExecutor3} from "../../contracts/lean/LeanExecutor3.sol";
import {BadPlan} from "../../contracts/lean/LeanPlanCodec.sol";
import {Hop3} from "../../contracts/lean/LeanPlan3Codec.sol";
import {Sized} from "../../contracts/lean/LeanSizer3.sol";
import {MockMorphoFlash} from "../../contracts/mocks/MockMorphoFlash.sol";
import {LeanToken, MockVault} from "./LeanMocks.sol";
import {MockCl3Pool, MockPoolKey3, MockPoolManager3} from "./Lean3Mocks.sol";
import {Sizer3Harness} from "./LeanSizer3.t.sol";

/// Task 8.5 R40.1: the third lean executor's run path, against pools that
/// price as the venues do. A trade is sized by the executor itself, borrowed,
/// swapped, floored at the transaction's own gas price, and paid out; any
/// reason not to trade returns before anything is borrowed.
contract LeanExecutor3RunTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    address constant MORPHO = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    address constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;

    bytes32 constant TRADED =
        keccak256("Traded(address,uint8,uint256,uint256,uint256,uint256,uint8,uint8)");
    bytes32 constant NO_TRADE = keccak256("NoTrade(address,uint8,uint8)");

    uint128 constant L = 1e21;
    uint128 constant CAP = 5e18;
    /// The mocks' lender work and the tail after the floor cost about 95k:
    /// their storage writes start from zero. Task 8 measures Base's.
    uint32 constant OVERHEAD = 120_000;
    uint32 constant SWAP_GAS = 300_000;
    uint64 constant L1_BOUND = 3e9;

    address owner = address(0xA11CE);
    address trader = address(0xB0B);
    address recipient = address(0xCAFE);
    LeanToken weth;
    LeanToken usdc;
    LeanToken x;
    MockPoolManager3 manager;
    Sizer3Harness sizer = new Sizer3Harness();
    LeanExecutor3 lean;

    struct Terms {
        uint8 lender;
        uint8 start;
        uint128 cap;
        uint128 rate;
        uint32 overhead;
        uint32 swapGas;
        uint64 l1Bound;
        uint64 minNet;
        uint8 maxSteps;
    }

    function setUp() public {
        vm.etch(WETH, address(new LeanToken()).code);
        vm.etch(USDC, address(new LeanToken()).code);
        weth = LeanToken(WETH);
        usdc = LeanToken(USDC);
        x = new LeanToken();
        vm.etch(VAULT, address(new MockVault()).code);
        vm.etch(MORPHO, address(new MockMorphoFlash()).code);
        vm.etch(POOL_MANAGER, address(new MockPoolManager3()).code);
        manager = MockPoolManager3(POOL_MANAGER);
        weth.mint(VAULT, 1_000e18);
        weth.mint(MORPHO, 1_000e18);
        usdc.mint(MORPHO, 1e15);
        weth.mint(POOL_MANAGER, 1e30);
        usdc.mint(POOL_MANAGER, 1e30);
        x.mint(POOL_MANAGER, 1e40);
        lean = new LeanExecutor3(owner, trader, recipient);
        vm.txGasPrice(10_000_000);
    }

    function terms() internal pure returns (Terms memory) {
        return Terms(0, 0, CAP, 0, OVERHEAD, SWAP_GAS, L1_BOUND, 1, 8);
    }

    function encode(Terms memory t, Hop3 memory a, Hop3 memory b) internal view returns (bytes memory p) {
        p = abi.encodePacked(
            t.lender, t.start, t.cap, uint40(block.timestamp + 10), t.rate, t.overhead, t.swapGas, t.l1Bound, t.minNet, t.maxSteps
        );
        p = abi.encodePacked(p, a.kind, a.pool, a.tokenOut, a.fee, a.tickSpacing, a.crossGas);
        p = abi.encodePacked(p, b.kind, b.pool, b.tokenOut, b.fee, b.tickSpacing, b.crossGas);
    }

    /// A pool between `start` and `x` that is `dear` (hop 0 sells the start
    /// there) or cheap (hop 1 buys it back), whatever the tokens' order.
    function pool(address start, bool dear, uint8 shape, uint24 fee, int24 spacing, uint128 liquidity)
        internal
        returns (MockCl3Pool p)
    {
        // Selling the start dear: its price in x is 120 ticks higher there. As
        // token1 its price is the pool's inverse, so the ticks turn over.
        int24 tick = dear ? int24(6_120) : int24(6_000);
        if (start > address(x)) tick = -tick;
        p = new MockCl3Pool(start, address(x), shape, tick, liquidity, fee, spacing);
        x.mint(address(p), 1e40);
        LeanToken(start).mint(address(p), 1e40);
    }

    function route(address start, uint128 liquidity) internal returns (Hop3 memory a, Hop3 memory b) {
        MockCl3Pool sell = pool(start, true, 0, 500, 60, liquidity);
        MockCl3Pool buy = pool(start, false, 1, 100, 60, liquidity);
        a = Hop3(0, address(sell), address(x), 500, 60, 30_000);
        b = Hop3(1, address(buy), start, 100, 60, 30_000);
    }

    function expected(Terms memory t, Hop3 memory a, Hop3 memory b, address start) internal view returns (Sized memory) {
        return sizer.size(a, b, start, t.cap, tx.gasprice, t.rate, t.maxSteps);
    }

    struct Seen {
        bool traded;
        uint256 amountIn;
        uint256 out;
        uint256 lenderFee;
        uint256 costWei;
        uint8 steps;
        uint8 flags;
        bool noTrade;
        uint8 reason;
    }

    function run(bytes memory p) internal returns (Seen memory s) {
        vm.recordLogs();
        vm.prank(trader);
        lean.run(p);
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i; i < logs.length; i++) {
            if (logs[i].emitter != address(lean)) continue;
            assertEq(address(uint160(uint256(logs[i].topics[1]))), trader, "the sender is the trader");
            if (logs[i].topics[0] == TRADED) {
                s.traded = true;
                uint8 start;
                (start, s.amountIn, s.out, s.lenderFee, s.costWei, s.steps, s.flags) =
                    abi.decode(logs[i].data, (uint8, uint256, uint256, uint256, uint256, uint8, uint8));
            } else if (logs[i].topics[0] == NO_TRADE) {
                s.noTrade = true;
                (s.reason, s.steps) = abi.decode(logs[i].data, (uint8, uint8));
            }
        }
    }

    function testTradesAtTheSizersSize() public {
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        Terms memory t = terms();
        Sized memory want = expected(t, a, b, WETH);
        assertGt(want.size, 0);
        Seen memory s = run(encode(t, a, b));
        assertTrue(s.traded);
        assertEq(s.amountIn, want.size);
        assertEq(s.out, want.out, "the swaps paid the sizer's quote");
        assertEq(weth.balanceOf(recipient), want.out - want.size, "the gross, to the recipient");
        assertEq(weth.balanceOf(VAULT), 1_000e18, "the vault was repaid");
        assertEq(weth.balanceOf(address(lean)), 0, "no WETH left");
        assertEq(x.balanceOf(address(lean)), 0, "no x left");
    }

    function testTheFloorCountsGasAtTheTransactionsPrice() public {
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        vm.txGasPrice(1e13);
        vm.expectCall(VAULT, abi.encodeWithSelector(MockVault.flashLoan.selector), 0);
        Seen memory s = run(encode(terms(), a, b));
        assertTrue(s.noTrade);
        assertEq(s.reason, lean.BELOW_FLOOR());
        assertEq(weth.balanceOf(recipient), 0);
    }

    /// Whatever the gas price, a trade let through nets at least its own
    /// cost, and that cost covers all the gas the call used: what `run` and
    /// its callback measured, and `gasOverhead` for the lender's part and the
    /// tail. Nothing is reverted on the way, since the prediction holds.
    function testFuzz_NetNeverBelowTheCostAtAnyGasPrice(uint64 price) public {
        price = uint64(bound(price, 1, 5e12));
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        vm.txGasPrice(price);
        Seen memory s = run(encode(terms(), a, b));
        uint256 used = vm.lastCallGas().gasTotalUsed;
        if (!s.traded) {
            assertEq(s.reason, lean.BELOW_FLOOR());
            return;
        }
        uint256 gross = s.out - s.amountIn - s.lenderFee;
        assertGe(gross, s.costWei + 1, "net of the floor's cost, and minNet");
        assertGe(s.costWei, uint256(price) * used + L1_BOUND, "the cost covers the gas the call used, and the L1 bound");
        assertLe(s.costWei, uint256(price) * (used + OVERHEAD) + L1_BOUND, "and does not count held-back gas");
        assertEq(weth.balanceOf(recipient), gross);
    }

    function testAUsdcCostIsConvertedAtTheRateRoundedUp() public {
        (Hop3 memory a, Hop3 memory b) = route(USDC, 1e13);
        Terms memory t = terms();
        (t.lender, t.start, t.cap, t.rate) = (1, 1, 12_000e6, 3_150_000_000);
        vm.txGasPrice(1e9);
        Seen memory s = run(encode(t, a, b));
        assertTrue(s.traded, "a USDC trade");
        uint256 cost = (s.costWei * 3_150_000_000 + 1e18 - 1) / 1e18;
        assertGe(s.out - s.amountIn, cost + 1);
        assertEq(usdc.balanceOf(recipient), s.out - s.amountIn);
        assertEq(usdc.balanceOf(MORPHO), 1e15, "Morpho pulled its loan back");
    }

    function testNoGapIsNoTradeAndBorrowsNothing() public {
        MockCl3Pool one = pool(WETH, false, 0, 500, 60, L);
        MockCl3Pool two = pool(WETH, false, 1, 100, 60, L);
        Hop3 memory a = Hop3(0, address(one), address(x), 500, 60, 30_000);
        Hop3 memory b = Hop3(1, address(two), WETH, 100, 60, 30_000);
        vm.expectCall(VAULT, abi.encodeWithSelector(MockVault.flashLoan.selector), 0);
        Seen memory s = run(encode(terms(), a, b));
        assertTrue(s.noTrade);
        assertEq(s.reason, lean.NO_GAP());
        assertEq(one.swaps() + two.swaps(), 0);
    }

    function testALenderHoldingLessIsNoTrade() public {
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        Terms memory t = terms();
        Sized memory want = expected(t, a, b, WETH);
        vm.prank(VAULT);
        weth.transfer(address(0xdead), 1_000e18 - want.size + 1);
        vm.expectCall(VAULT, abi.encodeWithSelector(MockVault.flashLoan.selector), 0);
        Seen memory s = run(encode(t, a, b));
        assertEq(s.reason, lean.LENDER_SHORT());
        assertTrue(s.noTrade);

        // Morpho's USDC.
        (a, b) = route(USDC, 1e13);
        (t.lender, t.start, t.cap, t.rate) = (1, 1, 12_000e6, 3_150_000_000);
        vm.prank(MORPHO);
        usdc.transfer(address(0xdead), 1e15 - 10);
        s = run(encode(t, a, b));
        assertEq(s.reason, lean.LENDER_SHORT());
    }

    function testAFloorMissedOnRealGasReverts() public {
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        Terms memory t = terms();
        (t.overhead, t.swapGas, t.l1Bound, t.minNet) = (0, 0, 0, 0);
        // A price at which the reads alone pass and the swaps' gas does not.
        Sized memory want = expected(t, a, b, WETH);
        uint256 profit = want.out - want.size;
        vm.txGasPrice(profit / 150_000);
        vm.prank(trader);
        (bool ok, bytes memory why) = address(lean).call(abi.encodeCall(LeanExecutor3.run, (encode(t, a, b))));
        assertFalse(ok, "the real gas misses the floor");
        assertEq(bytes4(why), LeanExecutor3.Floor.selector);
    }

    /// `minNet` holds on the real gas, not just the predicted: with the swaps'
    /// gas left out of the plan, the prediction passes and the floor does not.
    function testMinNetIsHeldOnTheRealGas() public {
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        Terms memory t = terms();
        (t.overhead, t.swapGas, t.l1Bound) = (0, 0, 0);
        Sized memory want = expected(t, a, b, WETH);
        uint256 profit = want.out - want.size;
        // About 300k gas in all, 100k of it before the loan: half the profit
        // in gas, then 60% of it as minNet, passes the prediction only.
        vm.txGasPrice(profit / 600_000);
        t.minNet = uint64(profit * 6 / 10);
        vm.prank(trader);
        (bool ok, bytes memory why) = address(lean).call(abi.encodeCall(LeanExecutor3.run, (encode(t, a, b))));
        assertFalse(ok, "the real gas and minNet miss the floor");
        assertEq(bytes4(why), LeanExecutor3.Floor.selector);
    }

    function testTheCapClampsAndIsFlagged() public {
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        Terms memory t = terms();
        t.cap = uint128(expected(t, a, b, WETH).size / 3);
        Seen memory s = run(encode(t, a, b));
        assertTrue(s.traded);
        assertEq(s.amountIn, t.cap);
        assertEq(s.flags & 1, 1, "capped");
    }

    function testTheStepLimitIsFlagged() public {
        // The pools meet near 6,060: a tick at 6,090 on the falling pool is
        // reached first, at spacing 10.
        MockCl3Pool sell = pool(WETH, true, 0, 500, 10, L);
        MockCl3Pool buy = pool(WETH, false, 1, 100, 60, L);
        int24 dear = WETH < address(x) ? int24(6_090) : int24(-6_090);
        sell.setTick(dear, 0);
        Hop3 memory a = Hop3(0, address(sell), address(x), 500, 10, 30_000);
        Hop3 memory b = Hop3(1, address(buy), WETH, 100, 60, 30_000);
        Terms memory t = terms();
        t.maxSteps = 1;
        Seen memory s = run(encode(t, a, b));
        assertTrue(s.traded);
        assertEq(s.flags & 2, 2, "truncated");
        assertEq(s.steps, 1);
    }

    function v4Route(address start) internal returns (Hop3 memory a, Hop3 memory b, MockPoolKey3 memory key) {
        bool startIsToken0 = start < address(x);
        (address c0, address c1) = startIsToken0 ? (start, address(x)) : (address(x), start);
        key = MockPoolKey3(c0, c1, 3_000, 60, address(0));
        manager.initialize(key, startIsToken0 ? int24(6_120) : int24(-6_120), L);
        a = Hop3(3, address(0), address(x), 3_000, 60, 25_000);
        MockCl3Pool buy = pool(start, false, 0, 500, 60, L);
        b = Hop3(0, address(buy), start, 500, 60, 30_000);
    }

    function testAV4RouteTradesUnderThePoolManager() public {
        (Hop3 memory a, Hop3 memory b,) = v4Route(WETH);
        Terms memory t = terms();
        t.lender = 2;
        Seen memory s = run(encode(t, a, b));
        assertTrue(s.traded);
        assertEq(weth.balanceOf(recipient), s.out - s.amountIn);
        assertEq(manager.nonzeroDeltas(), 0);
        assertFalse(manager.unlocked());
    }

    function testAV4ProtocolFeeIsNoTrade() public {
        (Hop3 memory a, Hop3 memory b, MockPoolKey3 memory key) = v4Route(WETH);
        manager.setProtocolFee(key, 1);
        Terms memory t = terms();
        t.lender = 2;
        Seen memory s = run(encode(t, a, b));
        assertEq(s.reason, lean.UNSUPPORTED());
        assertEq(manager.swaps(), 0);
    }

    function testAV4DeltaOfTheWrongSignReverts() public {
        (Hop3 memory a, Hop3 memory b,) = v4Route(WETH);
        manager.setFlipDelta(true);
        Terms memory t = terms();
        t.lender = 2;
        vm.expectRevert(LeanExecutor3.V4Delta.selector);
        vm.prank(trader);
        lean.run(encode(t, a, b));
    }

    function testALenderCallingBackTwiceIsRefused() public {
        vm.etch(VAULT, address(new DoubleVault3()).code);
        weth.mint(VAULT, 1_000e18);
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        vm.expectRevert(LeanExecutor3.UnexpectedCallback.selector);
        vm.prank(trader);
        lean.run(encode(terms(), a, b));
    }

    function testTheLendersFeeIsOwedAndCounted() public {
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        MockVault(VAULT).setFee(1e12);
        Seen memory s = run(encode(terms(), a, b));
        assertTrue(s.traded);
        assertEq(s.lenderFee, 1e12);
        assertEq(weth.balanceOf(recipient), s.out - s.amountIn - 1e12);
        assertEq(weth.balanceOf(VAULT), 1_000e18 + 1e12);
    }

    function testAPoolPayingLessThanItReportsReverts() public {
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        x.setShort(true);
        vm.prank(trader);
        (bool ok,) = address(lean).call(abi.encodeCall(LeanExecutor3.run, (encode(terms(), a, b))));
        assertFalse(ok);
    }

    function testExpiredRevertsBeforeAnyRead() public {
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        bytes memory p = encode(terms(), a, b);
        vm.warp(block.timestamp + 11);
        vm.expectRevert(abi.encodeWithSelector(LeanExecutor3.Expired.selector, block.timestamp - 1, block.timestamp));
        vm.prank(trader);
        lean.run(p);
    }

    function testNotTraderAndCircuitRevert() public {
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        bytes memory p = encode(terms(), a, b);
        vm.expectRevert(LeanExecutor3.NotTrader.selector);
        lean.run(p);
        vm.prank(owner);
        lean.tripCircuit();
        vm.expectRevert(LeanExecutor3.CircuitOpen.selector);
        vm.prank(trader);
        lean.run(p);
    }

    function testAPlanNotEndingInItsStartReverts() public {
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        Terms memory t = terms();
        t.start = 1;
        vm.expectRevert(BadPlan.selector);
        vm.prank(trader);
        lean.run(encode(t, a, b));
    }

    function testEveryCallbackOutsideALoanReverts() public {
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        bytes memory p = encode(terms(), a, b);
        vm.expectRevert(LeanExecutor3.UnexpectedCallback.selector);
        lean.receiveFlashLoan(new address[](1), new uint256[](1), new uint256[](1), p);
        vm.expectRevert(LeanExecutor3.UnexpectedCallback.selector);
        lean.onMorphoFlashLoan(1, p);
        vm.expectRevert(LeanExecutor3.UnexpectedCallback.selector);
        vm.prank(POOL_MANAGER);
        lean.unlockCallback(p);
        vm.expectRevert(LeanExecutor3.UnexpectedCallback.selector);
        lean.uniswapV3SwapCallback(1, 0, "");
        vm.expectRevert(LeanExecutor3.UnexpectedCallback.selector);
        lean.pancakeV3SwapCallback(1, 0, "");
    }

    function testAnUnlockCallbackFromALenderMidLoanReverts() public {
        vm.etch(VAULT, address(new HostileVault3()).code);
        weth.mint(VAULT, 1_000e18);
        (Hop3 memory a, Hop3 memory b) = route(WETH, L);
        vm.expectRevert(LeanExecutor3.UnexpectedCallback.selector);
        vm.prank(trader);
        lean.run(encode(terms(), a, b));
    }
}

/// A Balancer vault that lends, then calls back a second time for the same loan.
contract DoubleVault3 {
    function flashLoan(address recipient, address[] memory tokens, uint256[] memory amounts, bytes memory userData)
        external
    {
        LeanToken(tokens[0]).transfer(recipient, amounts[0]);
        uint256[] memory fees = new uint256[](1);
        LeanExecutor3(recipient).receiveFlashLoan(tokens, amounts, fees, userData);
        LeanExecutor3(recipient).receiveFlashLoan(tokens, amounts, fees, userData);
    }
}

/// A Balancer vault that, lending, calls the executor's unlock path instead.
contract HostileVault3 {
    function flashLoan(address recipient, address[] memory, uint256[] memory, bytes memory userData) external {
        LeanExecutor3(recipient).unlockCallback(userData);
    }
}
