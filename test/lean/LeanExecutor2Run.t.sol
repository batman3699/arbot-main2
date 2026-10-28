// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {LeanExecutor2} from "../../contracts/lean/LeanExecutor2.sol";
import {BadPlan} from "../../contracts/lean/LeanPlanCodec.sol";
import {LeanToken, MockV3Pool, MockVault, MockPoolManager} from "./LeanMocks.sol";
import {MockMorphoFlash} from "../../contracts/mocks/MockMorphoFlash.sol";

/// Task 8.5 R37: the second lean executor's run path, against venue doubles. A
/// trade borrows its start token (WETH or USDC) from Balancer, Morpho or
/// PoolManager's flash accounting, swaps hop by hop (a v4 hop through
/// PoolManager, settled at once), clears the floor in the start token, pays
/// the profit to the recipient and repays.
contract LeanExecutor2RunTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    address constant MORPHO = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    address constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;
    address constant HOOK = 0xB0b24B89dB0dafbE43C5b40226b63A179f592880;

    struct Hop {
        uint8 kind;
        address pool;
        address tokenOut;
        uint128 minOut;
        uint24 fee;
        int24 tickSpacing;
    }

    event Profit(uint256 gross);

    address owner = address(0xA11CE);
    address trader = address(0xB0B);
    address recipient = address(0xCAFE);
    LeanToken weth;
    LeanToken usdc;
    LeanToken axis;
    MockV3Pool wethToX;
    MockV3Pool xToWeth;
    LeanToken x;
    MockV3Pool usdcToAxis;
    MockV3Pool axisToUsdc;
    MockPoolManager manager;
    LeanExecutor2 lean;

    uint128 constant LOAN = 1e18;
    uint256 constant X_OUT = 2000e18;
    uint256 constant WETH_OUT = uint256(2000e18) / 1990;
    /// 1,000 USDC: 4,000 AXIS at 4 a dollar on one venue, 4,000 AXIS back at
    /// 3.98 a dollar on the other.
    uint128 constant USDC_LOAN = 1000e6;
    uint256 constant AXIS_OUT = uint256(USDC_LOAN) * 4e12;
    uint256 constant USDC_BACK = AXIS_OUT / 398e10;

    function setUp() public {
        vm.etch(WETH, address(new LeanToken()).code);
        vm.etch(USDC, address(new LeanToken()).code);
        weth = LeanToken(WETH);
        usdc = LeanToken(USDC);
        x = new LeanToken();
        axis = new LeanToken();
        vm.etch(VAULT, address(new MockVault()).code);
        vm.etch(MORPHO, address(new MockMorphoFlash()).code);
        vm.etch(POOL_MANAGER, address(new MockPoolManager()).code);
        manager = MockPoolManager(POOL_MANAGER);
        weth.mint(VAULT, 1000e18);
        usdc.mint(MORPHO, 1e12);
        usdc.mint(POOL_MANAGER, 1e12);
        axis.mint(POOL_MANAGER, 1e30);
        wethToX = new MockV3Pool(WETH, address(x), 2000, 1, false);
        xToWeth = new MockV3Pool(address(x), WETH, 1, 1990, true);
        x.mint(address(wethToX), 1e30);
        weth.mint(address(xToWeth), 1000e18);
        usdcToAxis = new MockV3Pool(USDC, address(axis), 4e12, 1, false);
        axisToUsdc = new MockV3Pool(address(axis), USDC, 1, 398e10, false);
        axis.mint(address(usdcToAxis), 1e30);
        usdc.mint(address(axisToUsdc), 1e12);
        manager.setRate(USDC, address(axis), 4e12, 1);
        manager.setRate(address(axis), USDC, 1, 398e10);
        lean = new LeanExecutor2(owner, trader, recipient);
    }

    function plan(uint8 lender, uint8 start, uint128 amount, uint128 minProfit, Hop[] memory hops)
        internal
        view
        returns (bytes memory p)
    {
        p = abi.encodePacked(lender, start, amount, uint40(block.timestamp + 60), minProfit);
        for (uint256 i; i < hops.length; i++) {
            p = abi.encodePacked(p, hops[i].kind, hops[i].pool, hops[i].tokenOut, hops[i].minOut);
            if (hops[i].kind == 3) p = abi.encodePacked(p, hops[i].fee, hops[i].tickSpacing);
        }
    }

    function v3(uint8 kind, address pool, address tokenOut, uint256 minOut) internal pure returns (Hop memory) {
        return Hop(kind, pool, tokenOut, uint128(minOut), 0, 0);
    }

    function v4(address tokenOut, uint256 minOut) internal pure returns (Hop memory) {
        return Hop(3, HOOK, tokenOut, uint128(minOut), 100, 1);
    }

    /// USDC lent by PoolManager, into AXIS on v4, back on v3.
    function v4ThenV3(uint256 v4Min) internal view returns (bytes memory) {
        Hop[] memory hops = new Hop[](2);
        hops[0] = v4(address(axis), v4Min);
        hops[1] = v3(0, address(axisToUsdc), USDC, USDC_LOAN + 1);
        return plan(2, 1, USDC_LOAN, 1, hops);
    }

    function testUsdcStartUnderMorphoPaysTheProfitInUsdc() public {
        Hop[] memory hops = new Hop[](2);
        hops[0] = v3(0, address(usdcToAxis), address(axis), AXIS_OUT);
        hops[1] = v3(0, address(axisToUsdc), USDC, USDC_LOAN + 1);
        vm.expectEmit(address(lean));
        emit Profit(USDC_BACK - USDC_LOAN);
        vm.prank(trader);
        lean.run(plan(1, 1, USDC_LOAN, 1, hops));
        assertEq(usdc.balanceOf(recipient), USDC_BACK - USDC_LOAN, "the gross, in USDC");
        assertEq(usdc.balanceOf(MORPHO), 1e12, "Morpho pulled its loan back");
        assertEq(usdc.balanceOf(address(lean)), 0);
        assertEq(axis.balanceOf(address(lean)), 0);
    }

    function testWethStartIsV1sTrade() public {
        Hop[] memory hops = new Hop[](2);
        hops[0] = v3(0, address(wethToX), address(x), X_OUT);
        hops[1] = v3(1, address(xToWeth), WETH, LOAN + 1);
        vm.expectEmit(address(lean));
        emit Profit(WETH_OUT - LOAN);
        vm.prank(trader);
        lean.run(plan(0, 0, LOAN, 1, hops));
        assertEq(weth.balanceOf(recipient), WETH_OUT - LOAN, "v1's gross for v1's route");
        assertEq(weth.balanceOf(VAULT), 1000e18);
    }

    function testV4HopUnderThePoolManager() public {
        vm.expectEmit(address(lean));
        emit Profit(USDC_BACK - USDC_LOAN);
        vm.prank(trader);
        lean.run(v4ThenV3(AXIS_OUT));
        assertEq(usdc.balanceOf(recipient), USDC_BACK - USDC_LOAN);
        assertEq(usdc.balanceOf(POOL_MANAGER), 1e12 + USDC_LOAN, "the loan back, and the v4 hop's input paid");
        assertEq(axis.balanceOf(POOL_MANAGER), 1e30 - AXIS_OUT, "the v4 hop's output taken");
        assertEq(usdc.balanceOf(address(lean)), 0);
        assertEq(axis.balanceOf(address(lean)), 0);
        (address c0, address c1, uint24 fee, int24 spacing, address hooks) = manager.lastKey();
        (address lo, address hi) = USDC < address(axis) ? (USDC, address(axis)) : (address(axis), USDC);
        assertEq(c0, lo, "currency0 is the lower address");
        assertEq(c1, hi);
        assertEq(fee, 100);
        assertEq(spacing, 1);
        assertEq(hooks, HOOK, "the pool field carries the hooks");
    }

    function testV4FirstOrSecond() public {
        Hop[] memory hops = new Hop[](2);
        hops[0] = v3(0, address(usdcToAxis), address(axis), AXIS_OUT);
        hops[1] = v4(USDC, USDC_LOAN + 1);
        vm.expectEmit(address(lean));
        emit Profit(USDC_BACK - USDC_LOAN);
        vm.prank(trader);
        lean.run(plan(2, 1, USDC_LOAN, 1, hops));
        assertEq(usdc.balanceOf(recipient), USDC_BACK - USDC_LOAN);
        assertEq(manager.swaps(), 1);
        assertEq(axis.balanceOf(address(lean)), 0);
    }

    function testTheUnlockReturnsWithNothingOwed() public {
        vm.prank(trader);
        lean.run(v4ThenV3(AXIS_OUT));
        assertEq(manager.nonzeroDeltas(), 0, "every delta settled");
        assertEq(manager.delta(USDC), 0);
        assertEq(manager.delta(address(axis)), 0);
        assertFalse(manager.unlocked(), "unlock returned");
    }

    function testV4HopUnderMorphoIsRefusedBeforeAnyLoan() public {
        Hop[] memory hops = new Hop[](2);
        hops[0] = v4(address(axis), AXIS_OUT);
        hops[1] = v3(0, address(axisToUsdc), USDC, USDC_LOAN + 1);
        bytes memory p = plan(1, 1, USDC_LOAN, 1, hops);
        vm.expectCall(MORPHO, abi.encodeWithSelector(MockMorphoFlash.flashLoan.selector), 0);
        vm.expectRevert(BadPlan.selector);
        vm.prank(trader);
        lean.run(p);
    }

    function testMalformedPlansRevertBeforeAnyLoan() public {
        Hop[] memory hops = new Hop[](2);
        hops[0] = v4(address(axis), AXIS_OUT);
        hops[1] = v3(0, address(axisToUsdc), USDC, USDC_LOAN + 1);
        bytes[] memory bad = new bytes[](3);
        bad[0] = plan(3, 1, USDC_LOAN, 1, hops);
        bad[1] = plan(2, 2, USDC_LOAN, 1, hops);
        hops[1].kind = 4;
        bad[2] = plan(2, 1, USDC_LOAN, 1, hops);
        for (uint256 i; i < bad.length; i++) {
            vm.expectRevert(BadPlan.selector);
            vm.prank(trader);
            lean.run(bad[i]);
        }
        assertEq(manager.swaps(), 0);
    }

    function testLastHopNotTheStartReverts() public {
        Hop[] memory hops = new Hop[](2);
        hops[0] = v3(0, address(usdcToAxis), address(axis), AXIS_OUT);
        hops[1] = v3(0, address(axisToUsdc), USDC, USDC_LOAN + 1);
        vm.expectRevert(BadPlan.selector);
        vm.prank(trader);
        lean.run(plan(1, 0, USDC_LOAN, 1, hops));
    }

    function testFloorRevertsInUsdc() public {
        Hop[] memory hops = new Hop[](2);
        hops[0] = v4(address(axis), AXIS_OUT);
        hops[1] = v3(0, address(axisToUsdc), USDC, USDC_LOAN + 1);
        uint128 minProfit = uint128(USDC_BACK - USDC_LOAN + 1);
        vm.expectRevert(abi.encodeWithSelector(LeanExecutor2.Floor.selector, USDC_BACK, USDC_LOAN + minProfit));
        vm.prank(trader);
        lean.run(plan(2, 1, USDC_LOAN, minProfit, hops));
    }

    function testHopShortOnAV4Hop() public {
        vm.expectRevert(abi.encodeWithSelector(LeanExecutor2.HopShort.selector, 0, AXIS_OUT, AXIS_OUT + 1));
        vm.prank(trader);
        lean.run(v4ThenV3(AXIS_OUT + 1));
    }

    function testAV4DeltaOfTheWrongSignReverts() public {
        manager.setFlipDelta(true);
        vm.expectRevert(LeanExecutor2.V4Delta.selector);
        vm.prank(trader);
        lean.run(v4ThenV3(AXIS_OUT));
    }

    function testAV4OutputOfTheWrongSignReverts() public {
        manager.setNegativeOut(true);
        vm.expectRevert(LeanExecutor2.V4Delta.selector);
        vm.prank(trader);
        lean.run(v4ThenV3(AXIS_OUT));
    }

    /// The loan flag is set while a lender runs: a lender that calls the
    /// unlock path itself is refused by the sender, not only the flag.
    function testAnUnlockCallbackFromALenderMidLoanReverts() public {
        vm.etch(VAULT, address(new HostileVault()).code);
        weth.mint(VAULT, 1000e18);
        Hop[] memory hops = new Hop[](2);
        hops[0] = v3(0, address(wethToX), address(x), X_OUT);
        hops[1] = v3(1, address(xToWeth), WETH, LOAN + 1);
        vm.expectRevert(LeanExecutor2.UnexpectedCallback.selector);
        vm.prank(trader);
        lean.run(plan(0, 0, LOAN, 1, hops));
    }

    function testUnlockCallbackFromAnyoneElseReverts() public {
        bytes memory p = v4ThenV3(AXIS_OUT);
        vm.expectRevert(LeanExecutor2.UnexpectedCallback.selector);
        lean.unlockCallback(p);
    }

    function testUnlockCallbackOutsideATradeReverts() public {
        bytes memory p = v4ThenV3(AXIS_OUT);
        vm.expectRevert(LeanExecutor2.UnexpectedCallback.selector);
        vm.prank(POOL_MANAGER);
        lean.unlockCallback(p);
    }

    function testNotTraderReverts() public {
        bytes memory p = v4ThenV3(AXIS_OUT);
        vm.expectRevert(LeanExecutor2.NotTrader.selector);
        lean.run(p);
    }

    function testCircuitOpenReverts() public {
        bytes memory p = v4ThenV3(AXIS_OUT);
        vm.prank(owner);
        lean.tripCircuit();
        vm.expectRevert(LeanExecutor2.CircuitOpen.selector);
        vm.prank(trader);
        lean.run(p);
    }

    function testExpiredRevertsBeforeAnyLoan() public {
        bytes memory p = v4ThenV3(AXIS_OUT);
        vm.warp(block.timestamp + 61);
        vm.expectRevert(abi.encodeWithSelector(LeanExecutor2.Expired.selector, block.timestamp - 1, block.timestamp));
        vm.prank(trader);
        lean.run(p);
        assertEq(manager.swaps(), 0);
    }
}

/// A Balancer vault that, lending, calls the executor's unlock path instead.
contract HostileVault {
    function flashLoan(address recipient, address[] memory, uint256[] memory, bytes memory userData) external {
        LeanExecutor2(recipient).unlockCallback(userData);
    }
}
