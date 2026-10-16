// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test, console} from "forge-std/Test.sol";
import {Vm} from "forge-std/Vm.sol";
import {LeanExecutor} from "../../contracts/lean/LeanExecutor.sol";

interface IERC20F {
    function balanceOf(address) external view returns (uint256);
    function approve(address, uint256) external returns (bool);
    function transfer(address, uint256) external returns (bool);
}

/// Uniswap's SwapRouter02 and PancakeSwap's SmartRouter share it.
interface IV3RouterNoDeadline {
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

interface ISlipstreamRouter {
    struct Params {
        address tokenIn;
        address tokenOut;
        int24 tickSpacing;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(Params calldata) external payable returns (uint256);
}

interface IAeroRouter {
    struct Route {
        address from;
        address to;
        bool stable;
        address factory;
    }

    function swapExactTokensForTokens(uint256, uint256, Route[] calldata, address, uint256)
        external
        returns (uint256[] memory);
}

interface IAeroPoolF {
    function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
}

interface ITickSpacing {
    function tickSpacing() external view returns (int24);
}

/// One hop, exposed: test-only.
contract LeanHopHarness is LeanExecutor {
    constructor(address o) LeanExecutor(o, o, o) {}

    function hopOnce(uint8 kind, address pool, address tokenIn, address tokenOut, uint256 amountIn, uint256 minOut)
        external
        returns (uint256)
    {
        return _hop(kind, pool, tokenIn, tokenOut, amountIn, minOut);
    }
}

/// Task 8.5 R29, on a fork of Base at block 52,671,989 (through
/// https://base.drpc.org, which serves historical state): ticket 664's route,
/// which the old executor paid 180,256,364,565,428 wei on at this block, and
/// every venue's hop against its own router.
///
///   FOUNDRY_PROFILE=fork forge test -vv
contract LeanExecutorForkTest is Test {
    uint256 constant BLOCK = 52_671_989;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant CBBTC = 0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;
    address constant OLD = 0x8940B565D050974b2b589B70De43bCb753b1F93B;
    address constant TRADER = 0xCB436Ba3acb945b3fc8EE6345857262584356595;
    uint256 constant GROSS = 180_256_364_565_428;

    /// Ticket 664's `startV2` calldata, as sent (tx 0xb1b7098a...5052).
    bytes constant TICKET_664 =
        hex"6290aa4800000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001a0000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000003123f852b8270497971d446e5bab4c47bbc9f917a87f34b7899352e3fcd249560000000000000000000000000000000000000000000000000000000000002105000000000000000000000000000000000000000000000000000000006ad1c8ce000000000000000000000000000000000000000000000000000000000000000100000000000000000000000042000000000000000000000000000000000000060000000000000000000000000000000000000000000000001ae361fc1451c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ba12222222228d8ba445958a75a0704d566bf2c80000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000002600000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000001c0000000000000000000000000000000000000000000000000000000000000000300000000000000000000000042000000000000000000000000000000000000060000000000000000000000000000000000000000000000001ae361fc1451c00000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000104a026383e0000000000000000000000004200000000000000000000000000000000000006000000000000000000000000cbb7c0000ab88b473b1f5afd9ef808440eed33bf00000000000000000000000000000000000000000000000000000000000000010000000000000000000000008940b565d050974b2b589b70de43bcb753b1f93b000000000000000000000000000000000000000000000000000000006ad1c8ce0000000000000000000000000000000000000000000000001ae361fc1451c0000000000000000000000000000000000000000000000000000000000000583b250000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000583b250000000000000000000000000000000000000000000000001ae361fc1451c001000000000000000000000000000000000000000000000000000000000000002bcbb7c0000ab88b473b1f5afd9ef808440eed33bf000bb84200000000000000000000000000000000000006000000000000000000000000000000000000000000";

    address recipient = address(0xCAFE);

    function setUp() public {
        vm.createSelectFork("https://base.drpc.org", BLOCK);
    }

    function route(uint8 lender) internal view returns (bytes memory) {
        return abi.encodePacked(
            lender,
            uint128(1_937_500_000_000_000_000),
            uint40(block.timestamp + 60),
            uint128(1),
            abi.encodePacked(uint8(0), address(0x7C7420DD105E2779316423Ba3E973f434315EFA9), CBBTC, uint128(5_782_309)),
            abi.encodePacked(uint8(0), address(0x8c7080564B5A792A33Ef2FD473fbA6364d5495e5), WETH, uint128(1_937_500_000_000_000_001))
        );
    }

    function runLean(uint8 lender, bool dust) internal returns (Vm.Gas memory gas) {
        return runLean(lender, dust, false);
    }

    /// `dust`: 1 unit of the middle token at the executor and 1 wei of WETH at
    /// the recipient, as seeded after deploy. `wethDust`: 1 wei of WETH at the
    /// executor as well.
    function runLean(uint8 lender, bool dust, bool wethDust) internal returns (Vm.Gas memory gas) {
        LeanExecutor lean = new LeanExecutor(address(this), address(this), recipient);
        if (dust) {
            deal(CBBTC, address(lean), 1);
            deal(WETH, recipient, 1);
        }
        if (wethDust) {
            deal(WETH, address(lean), 1);
        }
        uint256 before = IERC20F(WETH).balanceOf(recipient);
        lean.run(route(lender));
        gas = vm.lastCallGas();
        assertEq(IERC20F(WETH).balanceOf(recipient) - before, GROSS, "the old executor's gross, to the wei");
    }

    function testTicket664RouteIsExactAndLean() public {
        uint256 snap = vm.snapshotState();
        Vm.Gas memory cold = runLean(0, false);
        vm.revertToState(snap);
        Vm.Gas memory dusted = runLean(0, true);
        vm.revertToState(snap);

        vm.prank(TRADER);
        (bool ok, bytes memory ret) = OLD.call(TICKET_664);
        assertTrue(ok, "the old executor runs ticket 664 at this block");
        assertEq(abi.decode(ret, (uint256)), GROSS);
        Vm.Gas memory old = vm.lastCallGas();

        console.log("lean, cold:   used %s, refunded %s", cold.gasTotalUsed, uint256(int256(cold.gasRefunded)));
        console.log("lean, dusted: used %s, refunded %s", dusted.gasTotalUsed, uint256(int256(dusted.gasRefunded)));
        console.log("old:          used %s, refunded %s", old.gasTotalUsed, uint256(int256(old.gasRefunded)));
        assertLt(cold.gasTotalUsed, old.gasTotalUsed);
        assertLt(dusted.gasTotalUsed, cold.gasTotalUsed);
    }

    function testBalancerAgainstMorpho() public {
        uint256 snap = vm.snapshotState();
        Vm.Gas memory balancer = runLean(0, true);
        vm.revertToState(snap);
        Vm.Gas memory morpho = runLean(1, true);
        vm.revertToState(snap);
        Vm.Gas memory morphoWeth = runLean(1, true, true);
        console.log("balancer: used %s; morpho: used %s", balancer.gasTotalUsed, morpho.gasTotalUsed);
        console.log("morpho with WETH dust at the executor: used %s, refunded %s", morphoWeth.gasTotalUsed, uint256(int256(morphoWeth.gasRefunded)));
        console.log("morpho without: refunded %s", uint256(int256(morpho.gasRefunded)));
    }

    function testAnExpiredRunIsCheap() public {
        LeanExecutor lean = new LeanExecutor(address(this), address(this), recipient);
        bytes memory p = route(0);
        vm.warp(block.timestamp + 61);
        vm.expectRevert();
        lean.run(p);
        console.log("expired run: used %s", vm.lastCallGas().gasTotalUsed);
    }

    /// Each hop kind, on a real pool, returns what the venue's own router does
    /// for the same input at the same state.
    function testEachVenueSwapsAsItsRouterDoes() public {
        uint256 amountIn = 1e17;
        _v3(0, 0xd0b53D9277642d899DF5C87A3966A349A798F224, USDC, 0x2626664c2603336E57B271c5C0b26F421741e481, 500, amountIn);
        _v3(1, 0x72AB388E2E2F6FaceF59E3C3FA2C4E29011c2D38, USDC, 0x678Aa4bF4E210cf2166753e054d5b7c31cc7fa86, 100, amountIn);
        _slipstream(0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59, USDC, 0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5, amountIn);
        _slipstream(0x7C7420DD105E2779316423Ba3E973f434315EFA9, CBBTC, 0x698Cb2b6dd822994581fEa6eA4Fc755d1363A92F, amountIn);
        _aerodrome(0xcDAC0d6c6C59727a65F871236188350531885C43, USDC, 0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43, amountIn);
    }

    function _lean(uint8 kind, address pool, address tokenOut, uint256 amountIn, uint256 minOut) internal returns (uint256) {
        LeanHopHarness h = new LeanHopHarness(address(this));
        deal(WETH, address(h), amountIn);
        return h.hopOnce(kind, pool, WETH, tokenOut, amountIn, minOut);
    }

    function _v3(uint8 kind, address pool, address tokenOut, address router, uint24 fee, uint256 amountIn) internal {
        uint256 snap = vm.snapshotState();
        deal(WETH, address(this), amountIn);
        IERC20F(WETH).approve(router, amountIn);
        uint256 viaRouter = IV3RouterNoDeadline(router).exactInputSingle(
            IV3RouterNoDeadline.Params(WETH, tokenOut, fee, address(this), amountIn, 0, 0)
        );
        vm.revertToState(snap);
        assertEq(_lean(kind, pool, tokenOut, amountIn, 0), viaRouter, "a v3 hop and its router disagree");
        vm.revertToState(snap);
    }

    function _slipstream(address pool, address tokenOut, address router, uint256 amountIn) internal {
        uint256 snap = vm.snapshotState();
        deal(WETH, address(this), amountIn);
        IERC20F(WETH).approve(router, amountIn);
        uint256 viaRouter = ISlipstreamRouter(router).exactInputSingle(
            ISlipstreamRouter.Params(WETH, tokenOut, ITickSpacing(pool).tickSpacing(), address(this), block.timestamp, amountIn, 0, 0)
        );
        vm.revertToState(snap);
        assertEq(_lean(0, pool, tokenOut, amountIn, 0), viaRouter, "a Slipstream hop and its router disagree");
        vm.revertToState(snap);
    }

    function _aerodrome(address pool, address tokenOut, address router, uint256 amountIn) internal {
        uint256 snap = vm.snapshotState();
        deal(WETH, address(this), amountIn);
        IERC20F(WETH).approve(router, amountIn);
        IAeroRouter.Route[] memory r = new IAeroRouter.Route[](1);
        r[0] = IAeroRouter.Route(WETH, tokenOut, false, 0x420DD381b31aEf6683db6B902084cB0FFECe40Da);
        uint256[] memory amounts = IAeroRouter(router).swapExactTokensForTokens(amountIn, 0, r, address(this), block.timestamp);
        uint256 viaRouter = amounts[amounts.length - 1];
        vm.revertToState(snap);
        uint256 quote = IAeroPoolF(pool).getAmountOut(amountIn, WETH);
        assertEq(quote, viaRouter, "the pool's quote is what its router pays");
        assertEq(_lean(2, pool, tokenOut, amountIn, quote), viaRouter, "an Aerodrome hop and its router disagree");
        vm.revertToState(snap);
    }
}
