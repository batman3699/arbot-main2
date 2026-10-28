// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test, console} from "forge-std/Test.sol";
import {Vm} from "forge-std/Vm.sol";
import {LeanExecutor2} from "../../contracts/lean/LeanExecutor2.sol";

interface IERC20Fork2 {
    function balanceOf(address) external view returns (uint256);
    function approve(address, uint256) external returns (bool);
}

/// Uniswap's SwapRouter02 on Base: the trigger each test opens its gap with.
interface IV3RouterFork2 {
    struct Params {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(Params calldata) external payable returns (uint256);
}

/// Task 8.5 R37, on a fork of Base at block 53,155,787 (through
/// https://base.drpc.org, which serves historical state): the second lean
/// executor on AXIS's real pools, PoolManager lending USDC, and what a v4 hop
/// costs in gas. Each test opens its own gap first, with a swap on the v3
/// pool through Uniswap's router, as a trigger would.
///
/// drpc's free tier rate-limits a parallel run; throttle it and run one test at
/// a time (`--rpc-url` instead panics on Isthmus' L1 block at this foundry):
///
///   FOUNDRY_PROFILE=fork FOUNDRY_COMPUTE_UNITS_PER_SECOND=25 FOUNDRY_FORK_RETRIES=12 \
///   FOUNDRY_FORK_RETRY_BACKOFF=4000 forge test --match-path test/fork/LeanExecutor2Fork.t.sol -vv --threads 1
contract LeanExecutor2ForkTest is Test {
    uint256 constant BLOCK = 53_155_787;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant AXIS = 0x5bE75e3b0aFf5D1A042A5363Ae36AcB1d6d755ea;
    address constant ROUTER = 0x2626664c2603336E57B271c5C0b26F421741e481;
    address constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;
    /// AXIS/USDC on Uniswap v3, 500 ppm: the deeper of its two v3 pools here.
    address constant AXIS_V3 = 0xE1bfc5ADa1125718B53B2affC638Ba9981f6070F;
    uint24 constant AXIS_V3_FEE = 500;
    /// V4AlphaHook: AXIS's leading v4 pool (fee 100, spacing 1) is hooked by it.
    address constant HOOK = 0xB0b24B89dB0dafbE43C5b40226b63A179f592880;

    address recipient = address(0xCAFE);
    LeanExecutor2 lean;

    struct Hop {
        uint8 kind;
        address pool;
        address tokenOut;
        uint24 fee;
        int24 tickSpacing;
    }

    function setUp() public {
        vm.createSelectFork("https://base.drpc.org", BLOCK);
        lean = new LeanExecutor2(address(this), address(this), recipient);
    }

    function plan(uint8 lender, uint8 start, uint128 amount, Hop[] memory hops) internal view returns (bytes memory p) {
        p = abi.encodePacked(lender, start, amount, uint40(block.timestamp + 60), uint128(1));
        for (uint256 i; i < hops.length; i++) {
            p = abi.encodePacked(p, hops[i].kind, hops[i].pool, hops[i].tokenOut, uint128(0));
            if (hops[i].kind == 3) p = abi.encodePacked(p, hops[i].fee, hops[i].tickSpacing);
        }
    }

    /// A swap on AXIS's v3 pool: `usdcIn` USDC for AXIS (AXIS dearer there),
    /// or the same value of AXIS for USDC (AXIS cheaper there).
    function trigger(bool buyAxis, uint256 usdcIn) internal {
        address tokenIn = buyAxis ? USDC : AXIS;
        uint256 amountIn = usdcIn;
        if (!buyAxis) {
            // About the same notional in AXIS: ~4 AXIS a dollar here.
            amountIn = usdcIn * 4e12;
        }
        deal(tokenIn, address(this), amountIn);
        IERC20Fork2(tokenIn).approve(ROUTER, amountIn);
        IV3RouterFork2(ROUTER).exactInputSingle(
            IV3RouterFork2.Params(tokenIn, buyAxis ? AXIS : USDC, AXIS_V3_FEE, address(this), amountIn, 0, 0)
        );
    }

    /// Run `p`; the recipient's USDC must rise by exactly the Profit event's
    /// gross, and nothing may stay in the executor.
    function runAndCheck(bytes memory p) internal returns (uint256 gross, uint256 gas) {
        uint256 before = IERC20Fork2(USDC).balanceOf(recipient);
        vm.recordLogs();
        lean.run(p);
        gas = vm.lastCallGas().gasTotalUsed;
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bytes32 profit = keccak256("Profit(uint256)");
        bool seen;
        for (uint256 i; i < logs.length; i++) {
            if (logs[i].emitter == address(lean) && logs[i].topics[0] == profit) {
                gross = abi.decode(logs[i].data, (uint256));
                seen = true;
            }
        }
        assertTrue(seen, "Profit emitted");
        assertGt(gross, 0);
        assertEq(IERC20Fork2(USDC).balanceOf(recipient) - before, gross, "the gross, to the recipient");
        assertEq(IERC20Fork2(USDC).balanceOf(address(lean)), 0, "no USDC left");
        assertEq(IERC20Fork2(AXIS).balanceOf(address(lean)), 0, "no AXIS left");
    }

    function v4Then3(int24 spacing, uint24 fee, address hooks) internal view returns (bytes memory) {
        Hop[] memory hops = new Hop[](2);
        hops[0] = Hop(3, hooks, AXIS, fee, spacing);
        hops[1] = Hop(0, AXIS_V3, USDC, 0, 0);
        return plan(2, 1, 2_000e6, hops);
    }

    /// USDC lent by PoolManager, into AXIS on the hooked v4 pool, back on v3,
    /// after a buy on v3 made AXIS dearer there.
    function testAxisV4ThenV3() public {
        trigger(true, 20_000e6);
        (uint256 gross, uint256 gas) = runAndCheck(v4Then3(1, 100, HOOK));
        console.log("v4 (hooked) then v3: gross %s USDC units, gas %s", gross, gas);
    }

    /// The other way round, after a sale on v3 made AXIS cheaper there.
    function testAxisV3ThenV4() public {
        trigger(false, 20_000e6);
        Hop[] memory hops = new Hop[](2);
        hops[0] = Hop(0, AXIS_V3, AXIS, 0, 0);
        hops[1] = Hop(3, HOOK, USDC, 100, 1);
        (uint256 gross, uint256 gas) = runAndCheck(plan(2, 1, 2_000e6, hops));
        console.log("v3 then v4 (hooked): gross %s USDC units, gas %s", gross, gas);
    }

    /// The whole trade's gas through the hooked pool and through a hookless
    /// one (fee 8,200, spacing 82), each with the same v3 hop after the same
    /// trigger. Task 8 derives the v4 hop's base from these.
    function testV4HopGas() public {
        uint256 snap = vm.snapshotState();
        trigger(true, 20_000e6);
        (, uint256 hooked) = runAndCheck(v4Then3(1, 100, HOOK));
        vm.revertToState(snap);
        trigger(true, 20_000e6);
        uint256 before = IERC20Fork2(USDC).balanceOf(recipient);
        bytes memory p = v4Then3(82, 8200, address(0));
        // The 8,200 ppm pool may not pay after this trigger: its gas is read
        // whether or not the trade clears the floor.
        // The gas is read before anything else is called: `lastCallGas` is
        // the most recent call's.
        uint256 hookless;
        try lean.run(p) {
            hookless = vm.lastCallGas().gasTotalUsed;
            console.log("hookless trade cleared the floor, gross %s", IERC20Fork2(USDC).balanceOf(recipient) - before);
        } catch {
            hookless = vm.lastCallGas().gasTotalUsed;
            console.log("hookless trade reverted at the floor or a hop");
        }
        console.log("v4 hop gas: hooked trade %s, hookless trade %s", hooked, hookless);
    }

    /// v1's ticket-664 route, under the second executor and Morpho: v1's gross
    /// to the wei (LeanExecutorFork.t.sol), in WETH.
    function testWethRouteUnchanged() public {
        vm.createSelectFork("https://base.drpc.org", 52_671_989);
        LeanExecutor2 l2 = new LeanExecutor2(address(this), address(this), recipient);
        bytes memory p = abi.encodePacked(
            uint8(1),
            uint8(0),
            uint128(1_937_500_000_000_000_000),
            uint40(block.timestamp + 60),
            uint128(1),
            abi.encodePacked(
                uint8(0),
                address(0x7C7420DD105E2779316423Ba3E973f434315EFA9),
                address(0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf),
                uint128(5_782_309)
            ),
            abi.encodePacked(uint8(0), address(0x8c7080564B5A792A33Ef2FD473fbA6364d5495e5), WETH, uint128(1_937_500_000_000_000_001))
        );
        uint256 before = IERC20Fork2(WETH).balanceOf(recipient);
        l2.run(p);
        assertEq(IERC20Fork2(WETH).balanceOf(recipient) - before, 180_256_364_565_428, "v1's gross, to the wei");
    }
}
