// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test, Vm} from "forge-std/Test.sol";
import {LeanExecutor3} from "../../contracts/lean/LeanExecutor3.sol";
import {Hop3} from "../../contracts/lean/LeanPlan3Codec.sol";
import {Sized} from "../../contracts/lean/LeanSizer3.sol";
import {LeanToken, MockVault} from "./LeanMocks.sol";
import {MockCl3Pool} from "./Lean3Mocks.sol";
import {Sizer3Harness, Swapper3} from "./LeanSizer3.t.sol";

/// Task 8.5 R40.1: arms that share a trigger pool, run one after another in a
/// block. Each re-reads the pools, so a later one sizes only what the earlier
/// left; a gap already closed returns before borrowing. What is judged is the
/// portfolio's net, every included transaction's gas counted.
contract LeanExecutor3OverlapTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    bytes32 constant TRADED =
        keccak256("Traded(address,uint8,uint256,uint256,uint256,uint256,uint8,uint8)");
    bytes32 constant NO_TRADE = keccak256("NoTrade(address,uint8,uint8)");
    uint128 constant L = 1e21;
    uint256 constant PRICE = 10_000_000;

    address trader = address(0xB0B);
    address recipient = address(0xCAFE);
    LeanToken weth;
    LeanToken x;
    LeanExecutor3 lean;
    Sizer3Harness sizer = new Sizer3Harness();
    Swapper3 rival = new Swapper3();
    MockCl3Pool trigger;

    struct Result {
        bool traded;
        uint8 reason;
        uint256 gross;
        uint256 gasCost;
    }

    function setUp() public {
        vm.etch(WETH, address(new LeanToken()).code);
        vm.etch(USDC, address(new LeanToken()).code);
        weth = LeanToken(WETH);
        x = new LeanToken();
        vm.etch(VAULT, address(new MockVault()).code);
        weth.mint(VAULT, 1_000_000e18);
        lean = new LeanExecutor3(address(0xA11CE), trader, recipient);
        vm.txGasPrice(PRICE);
        // The trigger moved: WETH sells dear here, 240 ticks over the others.
        trigger = pool(true, 0, L);
    }

    function pool(bool dear, uint8 shape, uint128 liquidity) internal returns (MockCl3Pool p) {
        return poolAt(dear ? int24(6_240) : int24(6_000), shape, liquidity);
    }

    /// `tick` is WETH's price in x, whatever the tokens' order.
    function poolAt(int24 tick, uint8 shape, uint128 liquidity) internal returns (MockCl3Pool p) {
        if (WETH > address(x)) tick = -tick;
        p = new MockCl3Pool(WETH, address(x), shape, tick, liquidity, 500, 60);
        x.mint(address(p), 1e40);
        weth.mint(address(p), 1e40);
    }

    function hops(MockCl3Pool other) internal view returns (Hop3 memory a, Hop3 memory b) {
        a = Hop3(0, address(trigger), address(x), 500, 60, 30_000);
        b = Hop3(other.shape() == 1 ? 1 : 0, address(other), WETH, 500, 60, 30_000);
    }

    function plan(MockCl3Pool other) internal view returns (bytes memory p) {
        (Hop3 memory a, Hop3 memory b) = hops(other);
        p = abi.encodePacked(uint8(0), uint8(0), uint128(1_000e18), uint40(block.timestamp + 10), uint128(0));
        p = abi.encodePacked(p, uint32(120_000), uint32(300_000), uint64(3e9), uint64(1), uint8(8));
        p = abi.encodePacked(p, a.kind, a.pool, a.tokenOut, a.fee, a.tickSpacing, a.crossGas);
        p = abi.encodePacked(p, b.kind, b.pool, b.tokenOut, b.fee, b.tickSpacing, b.crossGas);
    }

    function sizeNow(MockCl3Pool other) internal view returns (Sized memory) {
        (Hop3 memory a, Hop3 memory b) = hops(other);
        return sizer.size(a, b, WETH, 1_000e18, PRICE, 0, 8);
    }

    /// One arm's transaction: what it netted the recipient, and what its gas
    /// cost, included either way.
    function fire(MockCl3Pool other) internal returns (Result memory r) {
        uint256 before = weth.balanceOf(recipient);
        bytes memory p = plan(other);
        vm.recordLogs();
        vm.prank(trader);
        lean.run(p);
        r.gasCost = (vm.lastCallGas().gasTotalUsed + 21_000) * PRICE;
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i; i < logs.length; i++) {
            if (logs[i].emitter != address(lean)) continue;
            if (logs[i].topics[0] == TRADED) r.traded = true;
            if (logs[i].topics[0] == NO_TRADE) (r.reason,) = abi.decode(logs[i].data, (uint8, uint8));
        }
        r.gross = weth.balanceOf(recipient) - before;
    }

    function net(Result memory r) internal pure returns (int256) {
        return int256(r.gross) - int256(r.gasCost);
    }

    function testASecondArmTradesOnlyWhatTheFirstLeft() public {
        MockCl3Pool one = pool(false, 0, L);
        MockCl3Pool two = pool(false, 1, L);
        Sized memory alone = sizeNow(two);
        Result memory first = fire(one);
        assertTrue(first.traded);
        Sized memory left = sizeNow(two);
        assertLt(left.size, alone.size, "the first trade moved the trigger back");
        assertGt(left.size, 0, "but left a gap on the second's pool");
        uint256 vaultCalls = MockVault(VAULT).calls();
        Result memory second = fire(two);
        assertTrue(second.traded);
        assertEq(MockVault(VAULT).calls(), vaultCalls + 1);
        assertGt(net(second), 0, "the remainder paid its own gas");
        assertGt(net(first) + net(second), net(first), "the portfolio beat the first alone");
    }

    function testAGapTheFirstClosedBorrowsNothing() public {
        // The deep pool's trade leaves the trigger just above 6,000 plus its
        // fees; WETH at 6,020 on the second pool is no cheaper than that.
        MockCl3Pool deep = pool(false, 0, L * 50);
        MockCl3Pool same = poolAt(6_020, 1, L);
        assertTrue(fire(deep).traded);
        uint256 vaultCalls = MockVault(VAULT).calls();
        Result memory second = fire(same);
        assertFalse(second.traded);
        assertTrue(second.reason == lean.NO_GAP() || second.reason == lean.BELOW_FLOOR());
        assertEq(MockVault(VAULT).calls(), vaultCalls, "nothing borrowed");
        assertEq(same.swaps(), 0);
    }

    function testTheSameRouteTwiceTheSecondTradesNothing() public {
        MockCl3Pool other = pool(false, 0, L);
        assertTrue(fire(other).traded);
        Result memory again = fire(other);
        assertFalse(again.traded, "a higher tier landing first leaves the lower none");
        assertEq(MockVault(VAULT).calls(), 1);
    }

    /// Twenty arms on one move: each that trades nets above its own gas, the
    /// rest borrow nothing, and the portfolio, every transaction's gas
    /// counted, nets at least the best single arm less the gas the others
    /// spent finding nothing.
    function testTwentyArmsOnOneMove() public {
        MockCl3Pool[] memory others = new MockCl3Pool[](20);
        for (uint256 i; i < 20; i++) {
            others[i] = pool(false, uint8(i % 2), L / 4 + uint128(i) * L / 20);
        }
        int256 bestAlone;
        uint256 snap = vm.snapshotState();
        for (uint256 i; i < 20; i++) {
            Result memory r = fire(others[i]);
            if (net(r) > bestAlone) bestAlone = net(r);
            vm.revertToState(snap);
        }
        int256 portfolio;
        int256 wasted;
        for (uint256 i; i < 20; i++) {
            Result memory r = fire(others[i]);
            portfolio += net(r);
            if (r.traded) {
                assertGt(r.gross, r.gasCost, "a trade nets above its gas");
            } else {
                wasted += int256(r.gasCost);
            }
        }
        emit log_named_int("best alone", bestAlone);
        emit log_named_int("portfolio", portfolio);
        emit log_named_int("spent finding nothing", wasted);
        assertGe(portfolio, bestAlone - wasted);
    }

    function testARivalBetweenOurArmsIsSizedAround() public {
        MockCl3Pool one = pool(false, 0, L);
        MockCl3Pool two = pool(false, 1, L);
        assertTrue(fire(one).traded);
        // A rival takes part of what is left, through the trigger.
        (Hop3 memory a,) = hops(two);
        weth.mint(address(rival), 1e18);
        rival.swap(a, WETH, 1e18);
        Sized memory want = sizeNow(two);
        Result memory second = fire(two);
        if (want.size == 0) {
            assertFalse(second.traded);
        } else {
            assertTrue(second.traded || second.reason == lean.BELOW_FLOOR());
        }
        assertGe(net(second), -int256(second.gasCost), "it can lose at most its gas");
    }
}
