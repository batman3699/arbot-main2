// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test, console} from "forge-std/Test.sol";
import {Vm} from "forge-std/Vm.sol";
import {LeanExecutor3} from "../../contracts/lean/LeanExecutor3.sol";
import {Hop3} from "../../contracts/lean/LeanPlan3Codec.sol";
import {Sized} from "../../contracts/lean/LeanSizer3.sol";
import {Sizer3Harness} from "../lean/LeanSizer3.t.sol";
import {ForkTrigger, IERC20Fork3} from "./LeanExecutor3Fork.t.sol";

interface IHookPoolFork3 {
    function fee() external view returns (uint24);
    function factory() external view returns (address);
}

interface IHookFactoryFork3 {
    function getPoolSwapHook(address pool) external view returns (address);
}

/// R44, on a fork of Base at block 58,230,400 (2026-11-16): `LeanExecutor3`
/// on a pool of Slipstream's third factory, `0x9cb2…`, whose fee comes from a
/// swap hook (`DynamicSwapFeeHook`) and is fixed for a block by its first
/// swap. The executor is unchanged: the pool swaps and calls back as
/// Slipstream's do, and hop kind 4 reads `fee()` inside the transaction,
/// which is what its swap then pays. Each test opens a gap, then runs the
/// executor: it must trade the size `LeanSizer3` computes from the pools as
/// they then stand, and the pools must pay exactly its quote. The gas is
/// logged; `apex_runtime::live::gas` is set from it.
///
///   FOUNDRY_PROFILE=fork FOUNDRY_COMPUTE_UNITS_PER_SECOND=25 FOUNDRY_FORK_RETRIES=12 \
///   FOUNDRY_FORK_RETRY_BACKOFF=4000 forge test --match-path test/fork/LeanExecutor3HookFork.t.sol -vv --threads 1
contract LeanExecutor3HookForkTest is Test {
    uint256 constant BLOCK = 58_230_400;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    /// WETH/USDC: Uniswap v3's 0.05% pool, and the hook factory's at spacing 50.
    address constant UNI = 0xd0b53D9277642d899DF5C87A3966A349A798F224;
    address constant HOOKED = 0x4A7af46bC8aBc00048c6c7920a1488DAF568366c;
    address constant FACTORY = 0x9CB220581b91b0C83223012FE69688b92aD07a76;
    address constant SWAP_HOOK = 0xC0ed888AfE87ed1AcF01D9f19bB850159e3ce434;
    bytes32 constant TRADED = keccak256("Traded(address,uint8,uint256,uint256,uint256,uint256,uint8,uint8)");
    uint256 constant PRICE = 10_000_000;
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
        return Hop3(4, HOOKED, tokenOut, 0, 50, 45_000);
    }

    function plan(Hop3 memory a, Hop3 memory b) internal view returns (bytes memory p) {
        p = abi.encodePacked(uint8(1), uint8(0), uint128(5e18), uint40(block.timestamp + 10), uint128(0));
        p = abi.encodePacked(p, uint32(OVERHEAD), uint32(330_000), uint64(3e9), uint64(1), uint8(8));
        p = abi.encodePacked(p, a.kind, a.pool, a.tokenOut, a.fee, a.tickSpacing, a.crossGas);
        p = abi.encodePacked(p, b.kind, b.pool, b.tokenOut, b.fee, b.tickSpacing, b.crossGas);
    }

    /// Buy WETH with USDC on `pool`, doubling until the route pays.
    function openGap(address pool, Hop3 memory a, Hop3 memory b) internal returns (Sized memory s) {
        uint256 usdcIn = 20_000e6;
        uint256 snap = vm.snapshotState();
        while (true) {
            deal(USDC, address(trigger), usdcIn);
            trigger.buy(hop(pool, WETH).kind, pool, USDC, WETH, usdcIn);
            s = sizer.size(a, b, WETH, 5e18, PRICE, 0, 8);
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
        Sized memory s = openGap(first, a, b);
        console.log("the hooked pool's fee as the trade runs: %s ppm", IHookPoolFork3(HOOKED).fee());
        uint256 before = IERC20Fork3(WETH).balanceOf(recipient);
        vm.recordLogs();
        vm.txGasPrice(PRICE);
        lean.run(plan(a, b));
        uint256 gas = vm.lastCallGas().gasTotalUsed;
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bool traded;
        for (uint256 i; i < logs.length; i++) {
            if (logs[i].emitter == address(lean) && logs[i].topics[0] == TRADED) {
                (, uint256 amountIn, uint256 out,,,,) =
                    abi.decode(logs[i].data, (uint8, uint256, uint256, uint256, uint256, uint8, uint8));
                assertEq(amountIn, s.size, "the executor's size is the sizer's");
                assertEq(out, s.out, "the pools paid the sizer's quote");
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

    /// The pool is the factory's, and its hook the one this was written against.
    function testThePoolIsTheFactorysAndItsHookTheReviewedOne() public view {
        assertEq(IHookPoolFork3(HOOKED).factory(), FACTORY);
        assertEq(IHookFactoryFork3(FACTORY).getPoolSwapHook(HOOKED), SWAP_HOOK);
    }

    /// The trigger is the block's first swap on the hooked pool: the fee is
    /// then the block's, fixed at the trigger's pre-swap tick, and ours pays it.
    function testHookedThenUni() public {
        tradeAndCheck(HOOKED, UNI);
    }

    /// The trigger is on Uniswap: ours is the hooked pool's first swap of the block.
    function testUniThenHooked() public {
        tradeAndCheck(UNI, HOOKED);
    }

    /// One swap's gas, each its own transaction: Uniswap's pool for reference,
    /// then the hooked pool's first swap of the block (the hook reads the TWAP
    /// and stores the block's fee) and its second (the fee is the stored one).
    /// `LEAN.hop.slipstream_hook` is Uniswap's figure plus the first swap's excess.
    function testSwapGasFirstAndSecondInTheBlock() public {
        deal(USDC, address(trigger), 3_000e6);
        trigger.buy(0, UNI, USDC, WETH, 1_000e6);
        uint256 uni = vm.lastCallGas().gasTotalUsed;
        trigger.buy(4, HOOKED, USDC, WETH, 1_000e6);
        uint256 first = vm.lastCallGas().gasTotalUsed;
        trigger.buy(4, HOOKED, USDC, WETH, 1_000e6);
        uint256 second = vm.lastCallGas().gasTotalUsed;
        console.log("swap gas: uniswap %s, hooked first %s, hooked second %s", uni, first, second);
        assertLt(second, first, "the block's fee is computed once");
    }

    /// With no gap, what a `NoTrade` costs with the hooked pool on either hop.
    function testNoTradeGas() public {
        lean.run(plan(hop(HOOKED, USDC), hop(UNI, WETH)));
        console.log("no trade hooked -> uni: gas %s", vm.lastCallGas().gasTotalUsed);
        lean.run(plan(hop(UNI, USDC), hop(HOOKED, WETH)));
        console.log("no trade uni -> hooked: gas %s", vm.lastCallGas().gasTotalUsed);
    }

    /// The hook's `afterSwap` wants 250,000 gas to spare for its tape. At the
    /// sized arms' limit the trade still runs, and pays the same.
    function testHookedAtTheGasLimit() public {
        Hop3 memory a = hop(HOOKED, USDC);
        Hop3 memory b = hop(UNI, WETH);
        Sized memory s = openGap(HOOKED, a, b);
        bytes memory p = plan(a, b);
        uint256 before = IERC20Fork3(WETH).balanceOf(recipient);
        vm.txGasPrice(PRICE);
        (bool ok,) = address(lean).call{gas: 1_000_000}(abi.encodeCall(LeanExecutor3.run, (p)));
        assertTrue(ok, "ran within a million gas");
        assertEq(IERC20Fork3(WETH).balanceOf(recipient) - before, s.out - s.size, "the same trade as with all gas");
    }
}
