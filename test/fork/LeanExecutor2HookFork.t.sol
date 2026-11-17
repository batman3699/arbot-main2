// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test, console} from "forge-std/Test.sol";
import {Vm} from "forge-std/Vm.sol";
import {LeanExecutor2} from "../../contracts/lean/LeanExecutor2.sol";
import {ForkTrigger, IERC20Fork3} from "./LeanExecutor3Fork.t.sol";

/// R44, on a fork of Base at block 58,230,400 (2026-11-16): `LeanExecutor2`,
/// the executor the live armed lane trades through, on a pool of Slipstream's
/// third factory (0x9CB2..., fee from `DynamicSwapFeeHook`). The pool is a
/// Slipstream pool to its caller: the executor's v3-style hop (kind 0) swaps
/// on it and pays in its callback, and the hook takes its fee inside the
/// swap. Both orders: the trigger on the hooked pool, so ours is the pool's
/// second swap of the block at the block's fee; and the trigger on Uniswap,
/// so ours is its first.
///
///   FOUNDRY_PROFILE=fork FOUNDRY_COMPUTE_UNITS_PER_SECOND=15 FOUNDRY_FORK_RETRIES=20 \
///   FOUNDRY_FORK_RETRY_BACKOFF=5000 forge test --match-path test/fork/LeanExecutor2HookFork.t.sol \
///   --match-test <name> -vv --threads 1
contract LeanExecutor2HookForkTest is Test {
    uint256 constant BLOCK = 58_230_400;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    /// WETH/USDC: Uniswap v3's 0.05% pool, and the hook factory's at spacing 50.
    address constant UNI = 0xd0b53D9277642d899DF5C87A3966A349A798F224;
    address constant HOOKED = 0x4A7af46bC8aBc00048c6c7920a1488DAF568366c;
    uint128 constant SIZE = 2e18;

    address recipient = address(0xCAFE);
    LeanExecutor2 lean;
    ForkTrigger trigger;

    function setUp() public {
        vm.createSelectFork("https://base.drpc.org", BLOCK);
        lean = new LeanExecutor2(address(this), address(this), recipient);
        trigger = new ForkTrigger();
    }

    /// Morpho lends `SIZE` WETH; sold on `first` for USDC, bought back on
    /// `second`; a floor of one wei of profit.
    function plan(address first, address second) internal view returns (bytes memory) {
        return abi.encodePacked(
            uint8(1),
            uint8(0),
            SIZE,
            uint40(block.timestamp + 60),
            uint128(1),
            abi.encodePacked(uint8(0), first, USDC, uint128(0)),
            abi.encodePacked(uint8(0), second, WETH, uint128(0))
        );
    }

    /// Buy WETH with USDC on `first` (WETH dearer there), doubling the buy
    /// until the route pays; then the trade, checked to the wei.
    function tradeAndCheck(address first, address second) internal {
        uint256 usdcIn = 250_000e6;
        uint256 snap = vm.snapshotState();
        while (true) {
            deal(USDC, address(trigger), usdcIn);
            trigger.buy(first == UNI ? 0 : 4, first, USDC, WETH, usdcIn);
            uint256 before = IERC20Fork3(WETH).balanceOf(recipient);
            vm.recordLogs();
            try lean.run(plan(first, second)) {
                uint256 gas = vm.lastCallGas().gasTotalUsed;
                Vm.Log[] memory logs = vm.getRecordedLogs();
                bytes32 profit = keccak256("Profit(uint256)");
                uint256 gross;
                for (uint256 i; i < logs.length; i++) {
                    if (logs[i].emitter == address(lean) && logs[i].topics[0] == profit) {
                        gross = abi.decode(logs[i].data, (uint256));
                    }
                }
                assertGt(gross, 0, "Profit emitted");
                assertEq(IERC20Fork3(WETH).balanceOf(recipient) - before, gross, "the gross, to the recipient");
                assertEq(IERC20Fork3(WETH).balanceOf(address(lean)), 0, "no WETH left");
                assertEq(IERC20Fork3(USDC).balanceOf(address(lean)), 0, "no USDC left");
                console.log("trigger %s USDC: gross %s wei, gas %s", usdcIn / 1e6, gross, gas);
                return;
            } catch {
                require(usdcIn < 40_000_000e6, "no gap opened");
                vm.revertToState(snap);
                snap = vm.snapshotState();
                usdcIn *= 2;
            }
        }
    }

    /// The trigger is the hooked pool's first swap of the block; ours pays
    /// the block's fee, fixed at the trigger's pre-swap tick.
    function testHookedThenUni() public {
        tradeAndCheck(HOOKED, UNI);
    }

    /// The trigger is on Uniswap: ours is the hooked pool's first swap of the block.
    function testUniThenHooked() public {
        tradeAndCheck(UNI, HOOKED);
    }
}
