// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test, console} from "forge-std/Test.sol";
import {Vm} from "forge-std/Vm.sol";
import {LeanExecutor3} from "../../contracts/lean/LeanExecutor3.sol";
import {Hop3} from "../../contracts/lean/LeanPlan3Codec.sol";
import {Sized} from "../../contracts/lean/LeanSizer3.sol";
import {Sizer3Harness} from "../lean/LeanSizer3.t.sol";

interface IERC20Fork3 {
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
}

interface IV3PoolFork3 {
    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 limit, bytes calldata data)
        external
        returns (int256, int256);
}

interface IAeroPoolFork3 {
    function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
    function token0() external view returns (address);
}

/// Opens each gap: buys a pool's token with the other, paying from what the
/// test deals it.
contract ForkTrigger {
    uint160 constant MIN_LIMIT = 4295128740;
    uint160 constant MAX_LIMIT = 1461446703485210103287273052203988822378723970341;
    address payToken;

    function buy(uint8 kind, address pool, address tokenIn, address tokenOut, uint256 amountIn) external {
        if (kind == 2) {
            uint256 out = IAeroPoolFork3(pool).getAmountOut(amountIn, tokenIn);
            IERC20Fork3(tokenIn).transfer(pool, amountIn);
            (uint256 o0, uint256 o1) = IAeroPoolFork3(pool).token0() == tokenIn ? (uint256(0), out) : (out, uint256(0));
            IAeroPoolFork3(pool).swap(o0, o1, address(this), "");
            return;
        }
        payToken = tokenIn;
        bool zeroForOne = tokenIn < tokenOut;
        IV3PoolFork3(pool).swap(address(this), zeroForOne, int256(amountIn), zeroForOne ? MIN_LIMIT : MAX_LIMIT, "");
    }

    function uniswapV3SwapCallback(int256 a0, int256 a1, bytes calldata) external {
        IERC20Fork3(payToken).transfer(msg.sender, uint256(a0 > 0 ? a0 : a1));
    }

    function pancakeV3SwapCallback(int256 a0, int256 a1, bytes calldata) external {
        IERC20Fork3(payToken).transfer(msg.sender, uint256(a0 > 0 ? a0 : a1));
    }
}

/// Task 8.5 R40.1, on a fork of Base at block 53,155,787 (through
/// https://base.drpc.org, which serves historical state): `LeanExecutor3` on
/// real pools. Each test opens a gap by buying WETH on its first pool, then
/// runs the executor: it must trade exactly the size `LeanSizer3` computes from
/// those pools, and the pools must pay exactly its quote. The gas each pair
/// uses is logged; `apex_runtime::live::gas::SIZED` is set from it.
///
/// drpc's free tier rate-limits a parallel run; throttle it and run one test at
/// a time:
///
///   FOUNDRY_PROFILE=fork FOUNDRY_COMPUTE_UNITS_PER_SECOND=25 FOUNDRY_FORK_RETRIES=12 \
///   FOUNDRY_FORK_RETRY_BACKOFF=4000 forge test --match-path test/fork/LeanExecutor3Fork.t.sol -vv --threads 1
///
/// Isolated (the profile's default), each call is its own transaction and its
/// gas is a transaction's, cold reads and all; but forge prices it at zero.
/// With FOUNDRY_ISOLATE=false the gas price applies, and the floor's cost is
/// checked against the gas the trade used.
contract LeanExecutor3ForkTest is Test {
    uint256 constant BLOCK = 53_155_787;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant AXIS = 0x5bE75e3b0aFf5D1A042A5363Ae36AcB1d6d755ea;
    /// WETH/USDC: Uniswap v3 500, PancakeSwap v3 100, Slipstream CL100, and
    /// Aerodrome v2's volatile pool.
    address constant UNI = 0xd0b53D9277642d899DF5C87A3966A349A798F224;
    address constant CAKE = 0x72AB388E2E2F6FaceF59E3C3FA2C4E29011c2D38;
    address constant SLIP = 0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59;
    address constant AERO = 0xcDAC0d6c6C59727a65F871236188350531885C43;
    /// AXIS/USDC on Uniswap v3, and its v4 pool's hook.
    address constant AXIS_V3 = 0xE1bfc5ADa1125718B53B2affC638Ba9981f6070F;
    address constant HOOK = 0xB0b24B89dB0dafbE43C5b40226b63A179f592880;
    bytes32 constant TRADED =
        keccak256("Traded(address,uint8,uint256,uint256,uint256,uint256,uint8,uint8)");
    uint256 constant PRICE = 10_000_000;
    /// `gasOverhead` under Morpho: the intrinsic gas and calldata, Morpho's
    /// work around the callback, and the tail after the floor.
    uint256 constant OVERHEAD = 110_000;

    address recipient = address(0xCAFE);
    LeanExecutor3 lean;
    Sizer3Harness sizer;
    ForkTrigger trigger;

    function setUp() public {
        vm.createSelectFork("https://base.drpc.org", BLOCK);
        lean = new LeanExecutor3(address(this), address(this), recipient);
        sizer = new Sizer3Harness();
        trigger = new ForkTrigger();
        vm.txGasPrice(PRICE);
    }

    function hop(address pool, address tokenOut) internal pure returns (Hop3 memory) {
        if (pool == UNI) return Hop3(0, UNI, tokenOut, 500, 10, 45_000);
        if (pool == CAKE) return Hop3(1, CAKE, tokenOut, 100, 1, 45_000);
        if (pool == SLIP) return Hop3(4, SLIP, tokenOut, 0, 100, 45_000);
        if (pool == AXIS_V3) return Hop3(0, AXIS_V3, tokenOut, 500, 10, 45_000);
        return Hop3(2, AERO, tokenOut, 0, 0, 0);
    }

    function plan(uint8 lender, uint8 start, uint128 cap, uint8 maxSteps, Hop3 memory a, Hop3 memory b)
        internal
        view
        returns (bytes memory p)
    {
        p = abi.encodePacked(lender, start, cap, uint40(block.timestamp + 10), uint128(0));
        p = abi.encodePacked(p, uint32(OVERHEAD), uint32(330_000), uint64(3e9), uint64(1), maxSteps);
        p = abi.encodePacked(p, a.kind, a.pool, a.tokenOut, a.fee, a.tickSpacing, a.crossGas);
        p = abi.encodePacked(p, b.kind, b.pool, b.tokenOut, b.fee, b.tickSpacing, b.crossGas);
    }

    /// Buy WETH with USDC on `pool`, doubling from 20,000 USDC until the
    /// route there and back pays: the smallest gap the executor trades.
    function openGap(address pool, Hop3 memory a, Hop3 memory b, uint8 maxSteps) internal returns (Sized memory s) {
        uint256 usdcIn = 20_000e6;
        uint256 snap = vm.snapshotState();
        while (true) {
            deal(USDC, address(trigger), usdcIn);
            trigger.buy(hop(pool, WETH).kind, pool, USDC, WETH, usdcIn);
            s = sizer.size(a, b, WETH, 5e18, PRICE, 0, maxSteps);
            if (s.size != 0 && s.out > s.size + 1e13) return s;
            require(usdcIn < 40_000_000e6, "no gap opened");
            vm.revertToState(snap);
            snap = vm.snapshotState();
            usdcIn *= 2;
        }
    }

    function tradeAndCheck(address first, address second) internal {
        Hop3 memory a = hop(first, USDC);
        Hop3 memory b = hop(second, WETH);
        Sized memory s = openGap(first, a, b, 8);
        uint256 before = IERC20Fork3(WETH).balanceOf(recipient);
        vm.recordLogs();
        // Each call is its own transaction here (`isolate`): the price is set
        // for this one.
        vm.txGasPrice(PRICE);
        lean.run(plan(1, 0, 5e18, 8, a, b));
        uint256 gas = vm.lastCallGas().gasTotalUsed;
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bool traded;
        for (uint256 i; i < logs.length; i++) {
            if (logs[i].emitter == address(lean) && logs[i].topics[0] == TRADED) {
                (, uint256 amountIn, uint256 out,, uint256 costWei,,) =
                    abi.decode(logs[i].data, (uint8, uint256, uint256, uint256, uint256, uint8, uint8));
                assertEq(amountIn, s.size, "the executor's size is the sizer's");
                assertEq(out, s.out, "the pools paid the sizer's quote");
                // gasOverhead covers the intrinsic gas and calldata (about 25k),
                // Morpho's own work, and the tail after the floor. Under
                // `isolate` forge prices each call at zero, so this binds only
                // in the run without it (FOUNDRY_ISOLATE=false).
                if (costWei > 3e9) {
                    assertGe(costWei, (gas + 25_000) * PRICE + 3e9, "the floor charged all the gas");
                    console.log("  overhead to spare %s", (costWei - 3e9) / PRICE - gas - 25_000);
                }
                traded = true;
            }
        }
        assertTrue(traded, "traded");
        assertEq(IERC20Fork3(WETH).balanceOf(recipient) - before, s.out - s.size);
        assertEq(IERC20Fork3(WETH).balanceOf(address(lean)), 0, "no WETH left");
        assertEq(IERC20Fork3(USDC).balanceOf(address(lean)), 0, "no USDC left");
        console.log("pair %s -> %s", first, second);
        console.log("  size %s, steps %s, gas %s", s.size, s.steps, gas);
    }

    function testUniThenCake() public {
        tradeAndCheck(UNI, CAKE);
    }

    function testCakeThenUni() public {
        tradeAndCheck(CAKE, UNI);
    }

    function testUniThenSlip() public {
        tradeAndCheck(UNI, SLIP);
    }

    function testSlipThenUni() public {
        tradeAndCheck(SLIP, UNI);
    }

    function testUniThenAero() public {
        tradeAndCheck(UNI, AERO);
    }

    function testAeroThenUni() public {
        tradeAndCheck(AERO, UNI);
    }

    function testCakeThenSlip() public {
        tradeAndCheck(CAKE, SLIP);
    }

    function testSlipThenCake() public {
        tradeAndCheck(SLIP, CAKE);
    }

    function testCakeThenAero() public {
        tradeAndCheck(CAKE, AERO);
    }

    function testAeroThenCake() public {
        tradeAndCheck(AERO, CAKE);
    }

    function testSlipThenAero() public {
        tradeAndCheck(SLIP, AERO);
    }

    function testAeroThenSlip() public {
        tradeAndCheck(AERO, SLIP);
    }

    /// The hooked v4 AXIS pool, under PoolManager's loan of USDC: AXIS made
    /// dearer on v3, sold there, bought back on v4 (and the other way).
    function v4Trade(bool v4First) internal {
        Hop3 memory v4 = Hop3(3, HOOK, v4First ? AXIS : USDC, 100, 1, 25_000);
        Hop3 memory v3 = hop(AXIS_V3, v4First ? USDC : AXIS);
        (Hop3 memory a, Hop3 memory b) = v4First ? (v4, v3) : (v3, v4);
        uint256 usdcIn = 2_000e6;
        Sized memory s;
        uint256 snap = vm.snapshotState();
        while (true) {
            // v4 first sells USDC for AXIS there and back on v3: AXIS must be
            // dearer on v3, so buy it there; v3 first, the opposite.
            if (v4First) {
                deal(USDC, address(trigger), usdcIn);
                trigger.buy(0, AXIS_V3, USDC, AXIS, usdcIn);
            } else {
                deal(AXIS, address(trigger), usdcIn * 4e12);
                trigger.buy(0, AXIS_V3, AXIS, USDC, usdcIn * 4e12);
            }
            s = sizer.size(a, b, USDC, 12_000e6, PRICE, 3_150_000_000, 8);
            if (s.size != 0 && s.out > s.size + 10_000) break;
            require(usdcIn < 1_000_000e6, "no gap opened");
            vm.revertToState(snap);
            snap = vm.snapshotState();
            usdcIn *= 2;
        }
        uint256 before = IERC20Fork3(USDC).balanceOf(recipient);
        bytes memory p = abi.encodePacked(uint8(2), uint8(1), uint128(12_000e6), uint40(block.timestamp + 10), uint128(3_150_000_000));
        p = abi.encodePacked(p, uint32(OVERHEAD), uint32(380_000), uint64(3e9), uint64(1), uint8(8));
        p = abi.encodePacked(p, a.kind, a.pool, a.tokenOut, a.fee, a.tickSpacing, a.crossGas);
        p = abi.encodePacked(p, b.kind, b.pool, b.tokenOut, b.fee, b.tickSpacing, b.crossGas);
        vm.txGasPrice(PRICE);
        lean.run(p);
        uint256 gas = vm.lastCallGas().gasTotalUsed;
        assertEq(IERC20Fork3(USDC).balanceOf(recipient) - before, s.out - s.size, "the pools paid the sizer's quote");
        console.log("v4 first %s: size %s USDC units, gas %s", v4First, s.size, gas);
        console.log("  steps %s", s.steps);
    }

    function testV4ThenV3() public {
        v4Trade(true);
    }

    function testV3ThenV4() public {
        v4Trade(false);
    }

    /// A move large enough to walk many boundaries: the gas at 8 steps, and
    /// what 16 would cost. Uniswap's spacing-10 pool against PancakeSwap's
    /// spacing-1 pool crosses the most. The trigger doubles from 200,000 USDC
    /// until a walk of 16 reaches 8 boundaries.
    function testGasAtMaxSteps() public {
        Hop3 memory a = hop(CAKE, USDC);
        Hop3 memory b = hop(UNI, WETH);
        uint256 usdcIn = 200_000e6;
        uint256 open = vm.snapshotState();
        while (true) {
            deal(USDC, address(trigger), usdcIn);
            trigger.buy(1, CAKE, USDC, WETH, usdcIn);
            if (sizer.size(a, b, WETH, 5e18, PRICE, 0, 16).steps >= 8) break;
            require(usdcIn < 6_400_000e6, "no walk of 8 boundaries");
            vm.revertToState(open);
            open = vm.snapshotState();
            usdcIn *= 2;
        }
        console.log("trigger %s USDC", usdcIn / 1e6);
        for (uint8 steps = 8; steps <= 16; steps += 8) {
            uint256 snap = vm.snapshotState();
            Sized memory s = sizer.size(a, b, WETH, 5e18, PRICE, 0, steps);
            vm.txGasPrice(PRICE);
            lean.run(plan(1, 0, 5e18, steps, a, b));
            uint256 gas = vm.lastCallGas().gasTotalUsed;
            console.log("max steps %s: steps %s, gas %s", steps, s.steps, gas);
            console.log("  size %s, flags %s", s.size, s.flags);
            assertLt(gas, 1_000_000, "within the sized arms' limit");
            vm.revertToState(snap);
        }
    }

    /// With no gap, what a `NoTrade` costs on each venue pair: the reads.
    function testNoTradeGasByPair() public {
        address[4] memory pools = [UNI, CAKE, SLIP, AERO];
        for (uint256 i; i < 4; i++) {
            for (uint256 j; j < 4; j++) {
                if (i == j) continue;
                lean.run(plan(1, 0, 5e18, 8, hop(pools[i], USDC), hop(pools[j], WETH)));
                console.log("no trade %s -> %s", pools[i], pools[j]);
                console.log("  gas %s", vm.lastCallGas().gasTotalUsed);
            }
        }
    }

    /// Slipstream's fee comes from its module under a 200,000 gas cap. At the
    /// sized arms' limit the fee, so the trade, is the same as with all gas.
    function testSlipstreamFeeAtTheGasLimit() public {
        Hop3 memory a = hop(SLIP, USDC);
        Hop3 memory b = hop(UNI, WETH);
        Sized memory s = openGap(SLIP, a, b, 8);
        bytes memory p = plan(1, 0, 5e18, 8, a, b);
        uint256 before = IERC20Fork3(WETH).balanceOf(recipient);
        vm.txGasPrice(PRICE);
        (bool ok,) = address(lean).call{gas: 1_000_000}(abi.encodeCall(LeanExecutor3.run, (p)));
        assertTrue(ok, "ran within a million gas");
        assertEq(IERC20Fork3(WETH).balanceOf(recipient) - before, s.out - s.size, "the same fee as with all gas");
    }
}
