// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {Hop3} from "../../contracts/lean/LeanPlan3Codec.sol";
import {Leg, LeanPools3} from "../../contracts/lean/LeanPools3.sol";
import {LeanSizer3, Sized} from "../../contracts/lean/LeanSizer3.sol";
import {LeanToken} from "./LeanMocks.sol";
import {
    MockAero3Pool, MockAeroFactory3, MockCl3Pool, MockPoolKey3, MockPoolManager3, MockSwapParams3
} from "./Lean3Mocks.sol";

contract Sizer3Harness {
    function size(Hop3 memory a, Hop3 memory b, address start, uint256 cap, uint256 weiPerGas, uint256 rate, uint256 maxSteps)
        external
        view
        returns (Sized memory)
    {
        Leg memory la;
        Leg memory lb;
        require(LeanPools3.load(a, start, la), "leg a");
        require(LeanPools3.load(b, a.tokenOut, lb), "leg b");
        return LeanSizer3.size(la, lb, cap, weiPerGas, rate, maxSteps);
    }
}

/// Swaps one hop as a trader would, to see what the pools really pay.
contract Swapper3 {
    address constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;
    uint160 constant MIN_LIMIT = 4295128740;
    uint160 constant MAX_LIMIT = 1461446703485210103287273052203988822378723970341;
    address tokenInNow;

    function swap(Hop3 memory h, address tokenIn, uint256 amount) external returns (uint256 out) {
        bool zeroForOne = tokenIn < h.tokenOut;
        if (h.kind == 2) {
            MockAero3Pool pool = MockAero3Pool(h.pool);
            out = pool.getAmountOut(amount, tokenIn);
            LeanToken(tokenIn).transfer(h.pool, amount);
            (uint256 o0, uint256 o1) = tokenIn == pool.token0() ? (uint256(0), out) : (out, uint256(0));
            pool.swap(o0, o1, address(this), "");
        } else if (h.kind == 3) {
            out = abi.decode(MockPoolManager3(POOL_MANAGER).unlock(abi.encode(h, tokenIn, amount)), (uint256));
        } else {
            tokenInNow = tokenIn;
            (int256 a0, int256 a1) =
                MockCl3Pool(h.pool).swap(address(this), zeroForOne, int256(amount), zeroForOne ? MIN_LIMIT : MAX_LIMIT, "");
            out = uint256(-(zeroForOne ? a1 : a0));
        }
    }

    function uniswapV3SwapCallback(int256 a0, int256 a1, bytes calldata) external {
        LeanToken(tokenInNow).transfer(msg.sender, uint256(a0 > 0 ? a0 : a1));
    }

    function pancakeV3SwapCallback(int256 a0, int256 a1, bytes calldata) external {
        LeanToken(tokenInNow).transfer(msg.sender, uint256(a0 > 0 ? a0 : a1));
    }

    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        (Hop3 memory h, address tokenIn, uint256 amount) = abi.decode(data, (Hop3, address, uint256));
        bool zeroForOne = tokenIn < h.tokenOut;
        (address c0, address c1) = zeroForOne ? (tokenIn, h.tokenOut) : (h.tokenOut, tokenIn);
        MockPoolManager3 m = MockPoolManager3(POOL_MANAGER);
        int256 d = m.swap(
            MockPoolKey3(c0, c1, h.fee, h.tickSpacing, h.pool),
            MockSwapParams3(zeroForOne, -int256(amount), zeroForOne ? MIN_LIMIT : MAX_LIMIT),
            ""
        );
        (int128 a0, int128 a1) = (int128(d >> 128), int128(d));
        (int128 paid, int128 got) = zeroForOne ? (a0, a1) : (a1, a0);
        m.sync(tokenIn);
        LeanToken(tokenIn).transfer(POOL_MANAGER, uint256(int256(-paid)));
        m.settle();
        m.take(h.tokenOut, address(this), uint256(int256(got)));
        return abi.encode(uint256(int256(got)));
    }
}

/// Task 8.5 R40.1: `LeanSizer3` against every case in
/// `crates/apex-exec/tests/fixtures/sizer3_cases.json`, which `apex_math::walk`
/// wrote (`crates/apex-math/tests/sizer3_cases.rs`): the size, the quoted
/// outputs, the boundaries and the flags, to the unit. Then the size is swapped
/// for real through pools that price as the venues do, and they must pay
/// exactly what the sizer quoted.
contract LeanSizer3Test is Test {
    address constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;
    string json;
    Sizer3Harness harness;
    Swapper3 swapper;
    MockAeroFactory3 factory;
    MockPoolManager3 manager;
    address t0;
    address t1;

    function setUp() public {
        json = vm.readFile("./crates/apex-exec/tests/fixtures/sizer3_cases.json");
        harness = new Sizer3Harness();
        swapper = new Swapper3();
        factory = new MockAeroFactory3();
        vm.etch(POOL_MANAGER, address(new MockPoolManager3()).code);
        manager = MockPoolManager3(POOL_MANAGER);
        address a = address(new LeanToken());
        address b = address(new LeanToken());
        (t0, t1) = a < b ? (a, b) : (b, a);
        LeanToken(t0).mint(POOL_MANAGER, 1e40);
        LeanToken(t1).mint(POOL_MANAGER, 1e40);
    }

    function str(string memory path) internal view returns (string memory) {
        return vm.parseJsonString(json, path);
    }

    function num(string memory path) internal view returns (uint256) {
        return vm.parseJsonUint(json, path);
    }

    function build(string memory c, uint256 i, address tokenIn, address tokenOut) internal returns (Hop3 memory) {
        string memory p = string.concat(c, ".pools[", vm.toString(i), "]");
        uint8 kind = uint8(num(string.concat(p, ".kind")));
        if (kind == 2) {
            uint256 rIn = vm.parseUint(str(string.concat(p, ".reserve_in")));
            uint256 rOut = vm.parseUint(str(string.concat(p, ".reserve_out")));
            (uint256 r0, uint256 r1) = tokenIn < tokenOut ? (rIn, rOut) : (rOut, rIn);
            MockAero3Pool pool = new MockAero3Pool(tokenIn, tokenOut, factory, r0, r1);
            factory.setFee(address(pool), num(string.concat(p, ".fee")) / 100);
            LeanToken(t0).mint(address(pool), r0);
            LeanToken(t1).mint(address(pool), r1);
            return Hop3(2, address(pool), tokenOut, 0, 0, 0);
        }
        int24 tick = int24(vm.parseJsonInt(json, string.concat(p, ".tick")));
        uint128 liquidity = uint128(vm.parseUint(str(string.concat(p, ".liquidity"))));
        uint24 fee = uint24(num(string.concat(p, ".fee")));
        int24 spacing = int24(vm.parseJsonInt(json, string.concat(p, ".spacing")));
        uint16 crossGas = uint16(num(string.concat(p, ".cross_gas")));
        int256[] memory ticks = vm.parseJsonIntArray(json, string.concat(p, ".ticks"));
        string[] memory nets = vm.parseJsonStringArray(json, string.concat(p, ".nets"));
        if (kind == 3) {
            address hooks = vm.parseJsonAddress(json, string.concat(p, ".hooks"));
            MockPoolKey3 memory key = MockPoolKey3(t0, t1, fee, spacing, hooks);
            manager.initialize(key, tick, liquidity);
            for (uint256 k; k < ticks.length; k++) {
                manager.setTick(key, int24(ticks[k]), int128(vm.parseInt(nets[k])));
            }
            return Hop3(3, hooks, tokenOut, fee, spacing, crossGas);
        }
        uint8 shape = kind == 1 ? 1 : kind == 4 ? 2 : 0;
        MockCl3Pool cl = new MockCl3Pool(t0, t1, shape, tick, liquidity, fee, spacing);
        for (uint256 k; k < ticks.length; k++) {
            cl.setTick(int24(ticks[k]), int128(vm.parseInt(nets[k])));
        }
        LeanToken(t0).mint(address(cl), 1e40);
        LeanToken(t1).mint(address(cl), 1e40);
        return Hop3(kind, address(cl), tokenOut, kind == 4 ? 0 : fee, spacing, crossGas);
    }

    function testEveryCaseEqualsTheWalkAndThePoolsPayItsQuote() public {
        string[] memory names = vm.parseJsonStringArray(json, ".names");
        assertGt(names.length, 40);
        for (uint256 n; n < names.length; n++) {
            uint256 snap = vm.snapshotState();
            string memory c = string.concat(".cases.", names[n]);
            string memory e = string.concat(c, ".expected");
            bool zeroForOne = vm.parseJsonBool(json, string.concat(c, ".zero_for_one"));
            (address start, address mid) = zeroForOne ? (t0, t1) : (t1, t0);
            Hop3 memory a = build(c, 0, start, mid);
            Hop3 memory b = build(c, 1, mid, start);
            Sized memory s = harness.size(
                a,
                b,
                start,
                vm.parseUint(str(string.concat(c, ".cap"))),
                vm.parseUint(str(string.concat(c, ".wei_per_gas"))),
                vm.parseUint(str(string.concat(c, ".rate"))),
                num(string.concat(c, ".max_steps"))
            );
            assertEq(s.size, vm.parseUint(str(string.concat(e, ".size"))), string.concat(names[n], " size"));
            assertEq(s.mid, vm.parseUint(str(string.concat(e, ".mid"))), string.concat(names[n], " mid"));
            assertEq(s.out, vm.parseUint(str(string.concat(e, ".out"))), string.concat(names[n], " out"));
            assertEq(s.steps, num(string.concat(e, ".steps")), string.concat(names[n], " steps"));
            assertEq(s.flags, num(string.concat(e, ".flags")), string.concat(names[n], " flags"));
            assertEq(s.crossGas, num(string.concat(e, ".cross_gas")), string.concat(names[n], " cross gas"));
            if (s.size != 0) {
                LeanToken(start).mint(address(swapper), s.size);
                uint256 got = swapper.swap(a, start, s.size);
                assertEq(got, s.mid, string.concat(names[n], ": the first pool paid its quote"));
                assertEq(swapper.swap(b, mid, got), s.out, string.concat(names[n], ": the second pool paid its quote"));
            }
            vm.revertToState(snap);
        }
    }
}
