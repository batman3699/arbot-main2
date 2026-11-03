// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {Hop3} from "../../contracts/lean/LeanPlan3Codec.sol";
import {Bound, Leg, LeanPools3} from "../../contracts/lean/LeanPools3.sol";
import {TickMath} from "../../contracts/libraries/TickMath.sol";
import {LeanToken} from "./LeanMocks.sol";
import {MockAero3Pool, MockAeroFactory3, MockCl3Pool, MockPoolKey3, MockPoolManager3} from "./Lean3Mocks.sol";

contract Pools3Harness {
    function load(Hop3 memory hop, address tokenIn) external view returns (bool ok, Leg memory leg) {
        ok = LeanPools3.load(hop, tokenIn, leg);
    }

    function bounds(Hop3 memory hop, address tokenIn, uint256 n) external view returns (Bound[] memory out) {
        Leg memory leg;
        LeanPools3.load(hop, tokenIn, leg);
        out = new Bound[](n);
        for (uint256 i; i < n; i++) {
            LeanPools3.nextBound(leg);
            out[i] = leg.bounds[i];
        }
    }

    function nets(Hop3 memory hop, address tokenIn, uint256 n) external view returns (int128[] memory out) {
        Leg memory leg;
        LeanPools3.load(hop, tokenIn, leg);
        out = new int128[](n);
        for (uint256 i; i < n; i++) {
            LeanPools3.nextBound(leg);
            out[i] = LeanPools3.netOf(leg, i);
            // Read twice: the second comes from the leg.
            LeanPools3.netOf(leg, i);
        }
    }
}

/// Slipstream's `slot0`, typed as its return really is.
interface ISlipstreamSlot0 {
    function slot0() external view returns (uint160, int24, uint16, uint16, uint16, bool);
}

/// Task 8.5 R40.1: `LeanPools3` reads each venue as the executor will, and
/// finds boundaries as v3's `nextInitializedTickWithinOneWord` does.
contract LeanPools3Test is Test {
    address constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;
    uint128 constant L = 1e24;
    Pools3Harness h = new Pools3Harness();
    LeanToken a = new LeanToken();
    LeanToken b = new LeanToken();
    address lo;
    address hi;

    function setUp() public {
        (lo, hi) = address(a) < address(b) ? (address(a), address(b)) : (address(b), address(a));
    }

    function hop(uint8 kind, address pool, address tokenOut, uint24 fee, int24 spacing) internal pure returns (Hop3 memory) {
        return Hop3(kind, pool, tokenOut, fee, spacing, 30_000);
    }

    function cl(uint8 shape, int24 tick, uint24 fee, int24 spacing) internal returns (MockCl3Pool) {
        return new MockCl3Pool(lo, hi, shape, tick, L, fee, spacing);
    }

    function testV3ShapedSlot0ReadsSqrtAndTick() public {
        MockCl3Pool p = cl(0, -12_345, 500, 5);
        (bool ok, Leg memory leg) = h.load(hop(0, address(p), hi, 500, 5), lo);
        assertTrue(ok);
        assertEq(leg.sqrtP, TickMath.getSqrtRatioAtTick(-12_345));
        assertEq(leg.tick, -12_345);
        assertEq(leg.liquidity, L);
        assertTrue(leg.zeroForOne);
    }

    function testSlipstreamShapedSlot0ReadsTheSame() public {
        MockCl3Pool p = cl(2, 7_777, 500, 1);
        (bool ok, Leg memory leg) = h.load(hop(4, address(p), lo, 0, 1), hi);
        assertTrue(ok);
        assertEq(leg.tick, 7_777);
        assertFalse(leg.zeroForOne);
    }

    function testATypedSlipstreamSlot0RevertsOnUniswaps() public {
        MockCl3Pool p = cl(0, 100, 500, 10);
        // Uniswap's sixth word is a fee, 0x44: no bool. The decoder refuses it
        // once the value is used (`via_ir` skips decoding an unused one).
        vm.expectRevert();
        this.unlockedOf(address(p));
    }

    function unlockedOf(address pool) external view returns (bool unlocked) {
        (,,,,, unlocked) = ISlipstreamSlot0(pool).slot0();
        require(unlocked || !unlocked);
    }

    function testSlipstreamFeeIsReadAtExecution() public {
        MockCl3Pool p = cl(2, 0, 500, 1);
        p.setFee(777);
        (, Leg memory leg) = h.load(hop(4, address(p), hi, 0, 1), lo);
        assertEq(leg.feePpm, 777);
        p.setFee(150);
        (, leg) = h.load(hop(4, address(p), hi, 0, 1), lo);
        assertEq(leg.feePpm, 150);
    }

    function testPancakeFeeComesFromThePlan() public {
        MockCl3Pool p = cl(1, 0, 2_500, 50);
        (, Leg memory leg) = h.load(hop(1, address(p), hi, 100, 50), lo);
        assertEq(leg.feePpm, 100);
    }

    function testAeroFeeBpsBecomesPpmAndReservesFaceTheTrade() public {
        MockAeroFactory3 f = new MockAeroFactory3();
        MockAero3Pool p = new MockAero3Pool(lo, hi, f, 1_000e18, 3_000_000e6);
        f.setFee(address(p), 30);
        (bool ok, Leg memory leg) = h.load(hop(2, address(p), lo, 0, 0), hi);
        assertTrue(ok);
        assertEq(leg.feePpm, 3_000);
        assertEq(leg.reserveIn, 3_000_000e6, "selling token1");
        assertEq(leg.reserveOut, 1_000e18);
    }

    function testAnEmptyAeroPoolIsRefused() public {
        MockAeroFactory3 f = new MockAeroFactory3();
        MockAero3Pool p = new MockAero3Pool(lo, hi, f, 0, 1e18);
        (bool ok,) = h.load(hop(2, address(p), hi, 0, 0), lo);
        assertFalse(ok);
    }

    function manager() internal returns (MockPoolManager3 m) {
        vm.etch(POOL_MANAGER, address(new MockPoolManager3()).code);
        m = MockPoolManager3(POOL_MANAGER);
    }

    function testV4WordsDecode() public {
        MockPoolManager3 m = manager();
        MockPoolKey3 memory key = MockPoolKey3(lo, hi, 3_000, 60, address(0));
        m.initialize(key, -5_040, L);
        (bool ok, Leg memory leg) = h.load(hop(3, address(0), hi, 3_000, 60), lo);
        assertTrue(ok);
        assertEq(leg.tick, -5_040);
        assertEq(leg.sqrtP, TickMath.getSqrtRatioAtTick(-5_040));
        assertEq(leg.liquidity, L);
        assertEq(leg.feePpm, 3_000);
    }

    function testV4ProtocolFeeRefused() public {
        MockPoolManager3 m = manager();
        MockPoolKey3 memory key = MockPoolKey3(lo, hi, 3_000, 60, address(0));
        m.initialize(key, 0, L);
        m.setProtocolFee(key, 1);
        (bool ok,) = h.load(hop(3, address(0), hi, 3_000, 60), lo);
        assertFalse(ok);
    }

    function testV4DynamicFeeRefused() public {
        MockPoolManager3 m = manager();
        MockPoolKey3 memory key = MockPoolKey3(lo, hi, 0x800000, 60, address(0));
        m.initialize(key, 0, L);
        (bool ok,) = h.load(hop(3, address(0), hi, 0x800000, 60), lo);
        assertFalse(ok);
    }

    function testV4DeltaReturningHookRefused() public {
        MockPoolManager3 m = manager();
        address[2] memory hooks = [address(uint160(0x1000 | (1 << 3))), address(uint160(0x1000 | (1 << 2)))];
        for (uint256 i; i < 2; i++) {
            MockPoolKey3 memory key = MockPoolKey3(lo, hi, 3_000, 60, hooks[i]);
            m.initialize(key, 0, L);
            (bool ok,) = h.load(hop(3, hooks[i], hi, 3_000, 60), lo);
            assertFalse(ok);
        }
    }

    function testV4NoDeltaHookLoads() public {
        MockPoolManager3 m = manager();
        // beforeSwap and afterSwap, neither returning a delta.
        address hook = address(uint160(0x1000 | (1 << 7) | (1 << 6)));
        MockPoolKey3 memory key = MockPoolKey3(lo, hi, 3_000, 60, hook);
        m.initialize(key, 0, L);
        (bool ok,) = h.load(hop(3, hook, hi, 3_000, 60), lo);
        assertTrue(ok);
    }

    function testZeroLiquidityRefused() public {
        MockCl3Pool p = new MockCl3Pool(lo, hi, 0, 0, 0, 500, 10);
        (bool ok,) = h.load(hop(0, address(p), hi, 500, 10), lo);
        assertFalse(ok);
    }

    /// v3's answers, worked by hand: initialized ticks found inside a word,
    /// word edges where there are none, both directions, negative ticks.
    function testBoundsFollowNextInitializedTickWithinOneWord() public {
        MockCl3Pool p = cl(0, 6_000, 500, 60);
        p.setTick(5_940, int128(int256(uint256(L / 2))));
        p.setTick(6_180, int128(int256(uint256(L / 2))));
        // Falling from 6,000 (compressed 100): 5,940 (99), then that word's
        // lowest tick, 0, then the next word down's lowest, -15,360.
        Bound[] memory down = h.bounds(hop(0, address(p), hi, 500, 60), lo, 3);
        assertEq(down[0].tick, 5_940);
        assertTrue(down[0].initialized);
        assertEq(down[1].tick, 0);
        assertFalse(down[1].initialized);
        assertEq(down[2].tick, -15_360);
        // Rising: 6,180 (103), then the word's highest, 255 × 60.
        Bound[] memory up = h.bounds(hop(0, address(p), lo, 500, 60), hi, 2);
        assertEq(up[0].tick, 6_180);
        assertTrue(up[0].initialized);
        assertEq(up[1].tick, 15_300);
        assertEq(up[1].sqrtPrice, TickMath.getSqrtRatioAtTick(15_300));

        // Spacing 1 at -1: word -1, bit 255. Falling ends at -256; rising
        // starts from 0 (word 0, bit 0) and ends at 255.
        MockCl3Pool q = cl(0, -1, 100, 1);
        assertEq(h.bounds(hop(0, address(q), hi, 100, 1), lo, 1)[0].tick, -256);
        assertEq(h.bounds(hop(0, address(q), lo, 100, 1), hi, 1)[0].tick, 255);
        // An unspaced negative tick rounds its compression down: -7 at
        // spacing 10 is compressed -1, so falling ends at -2,560.
        MockCl3Pool r = cl(0, -7, 500, 10);
        assertEq(h.bounds(hop(0, address(r), hi, 500, 10), lo, 1)[0].tick, -2_560);
    }

    function testABoundExactlyOnTheTickIsTheFirstFallingOne() public {
        MockCl3Pool p = cl(0, 6_000, 500, 60);
        p.setTick(6_000, int128(int256(uint256(L / 2))));
        Bound[] memory down = h.bounds(hop(0, address(p), hi, 500, 60), lo, 2);
        assertEq(down[0].tick, 6_000, "lte includes the current tick");
        assertEq(down[1].tick, 0, "then from 5,999 on");
        Bound[] memory up = h.bounds(hop(0, address(p), lo, 500, 60), hi, 1);
        assertEq(up[0].tick, 15_300, "rising skips it");
    }

    function testBitmapWordIsReadOnce() public {
        MockCl3Pool p = cl(0, 6_000, 500, 60);
        p.setTick(5_940, 1);
        p.setTick(5_880, 1);
        vm.expectCall(address(p), abi.encodeWithSignature("tickBitmap(int16)", int16(0)), 1);
        h.bounds(hop(0, address(p), hi, 500, 60), lo, 3);
    }

    function testMinAndMaxTickClamp() public {
        MockCl3Pool p = cl(0, TickMath.MIN_TICK + 5, 100, 1);
        assertEq(h.bounds(hop(0, address(p), hi, 100, 1), lo, 1)[0].tick, TickMath.MIN_TICK);
        MockCl3Pool q = cl(0, TickMath.MAX_TICK - 5, 100, 1);
        assertEq(h.bounds(hop(0, address(q), lo, 100, 1), hi, 1)[0].tick, TickMath.MAX_TICK);
    }

    function testNetIsReadOnceAndOnlyWhenInitialized() public {
        MockCl3Pool p = cl(2, 6_000, 500, 60);
        p.setTick(5_940, -123);
        vm.expectCall(address(p), abi.encodeWithSignature("ticks(int24)", int24(5_940)), 1);
        int128[] memory n = h.nets(hop(4, address(p), hi, 0, 60), lo, 2);
        assertEq(n[0], -123);
        assertEq(n[1], 0, "a word edge has none");
    }

    function testV4BoundsAndNetsReadThroughExtsload() public {
        MockPoolManager3 m = manager();
        MockPoolKey3 memory key = MockPoolKey3(lo, hi, 500, 10, address(0));
        m.initialize(key, -3_000, L);
        m.setTick(key, -3_050, -456);
        m.setTick(key, -2_900, 789);
        Bound[] memory down = h.bounds(hop(3, address(0), hi, 500, 10), lo, 2);
        assertEq(down[0].tick, -3_050);
        assertTrue(down[0].initialized);
        assertEq(h.nets(hop(3, address(0), hi, 500, 10), lo, 1)[0], -456);
        assertEq(h.bounds(hop(3, address(0), lo, 500, 10), hi, 1)[0].tick, -2_900);
        assertEq(h.nets(hop(3, address(0), lo, 500, 10), hi, 1)[0], 789);
    }
}
