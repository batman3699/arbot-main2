// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test, console} from "forge-std/Test.sol";
import {Vm} from "forge-std/Vm.sol";
import {LeanExecutor3} from "../../contracts/lean/LeanExecutor3.sol";
import {Hop3} from "../../contracts/lean/LeanPlan3Codec.sol";
import {Sized} from "../../contracts/lean/LeanSizer3.sol";
import {Sizer3Harness} from "../lean/LeanSizer3.t.sol";
import {ForkTrigger, IERC20Fork3} from "./LeanExecutor3Fork.t.sol";

interface IExtsloadFork3 {
    function extsload(bytes32 slot) external view returns (bytes32);
}

/// R51, on a fork of Base at block 59,821,700 (through https://base.drpc.org):
/// target route #2, NOCK/USDC on Uniswap v4 (a 30,000 LP fee, and 1,000 pips
/// of protocol fee each way since 2026-11-19) against Aerodrome v2's volatile
/// pool. Each test opens a gap on Aerodrome, then runs the executor under
/// PoolManager's loan of USDC: it must trade exactly the size `LeanSizer3`
/// computes, and the pools must pay exactly its quote, which holds only at the
/// composed fee, 30,970. At the LP fee alone the quote is more than
/// PoolManager pays. The gas is logged.
///
///   FOUNDRY_PROFILE=fork FOUNDRY_COMPUTE_UNITS_PER_SECOND=25 FOUNDRY_FORK_RETRIES=12 \
///   FOUNDRY_FORK_RETRY_BACKOFF=4000 forge test --match-path test/fork/LeanExecutor3ProtocolFeeFork.t.sol -vv --threads 1
contract LeanExecutor3ProtocolFeeForkTest is Test {
    uint256 constant BLOCK = 59_821_700;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant NOCK = 0x9B5E262cF9bb04869ab40b19AF91D2dc85761722;
    address constant AERO = 0x85f1Aa3a70fEdd1C52705c15BAEd143e675CD626;
    address constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;
    bytes32 constant POOL_ID = 0x064a5df2ecfcb7c05779b147aa38e4b0e91c966267fbdbc8cf16addd83636b13;
    bytes32 constant TRADED =
        keccak256("Traded(address,uint8,uint256,uint256,uint256,uint256,uint8,uint8)");
    uint256 constant PRICE = 10_000_000;
    /// USDC units a 1e18 wei: about $1,800 an ETH at the block.
    uint128 constant RATE = 1_890_000_000;
    uint128 constant CAP = 12_000e6;
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

    /// The pool is the one the runtime's book composes: its key hashes to the
    /// ID, and its slot0 holds the LP fee and the protocol fee.
    function testThePoolHasItsProtocolFeeOn() public view {
        assertEq(keccak256(abi.encode(USDC, NOCK, uint24(30_000), int24(300), address(0))), POOL_ID);
        uint256 word = uint256(IExtsloadFork3(POOL_MANAGER).extsload(keccak256(abi.encode(POOL_ID, uint256(6)))));
        assertEq(uint24(word >> 184), 4_097_000, "1,000 pips each way");
        assertEq(uint24(word >> 208), 30_000, "the LP fee");
    }

    function v4Hop(address tokenOut) internal pure returns (Hop3 memory) {
        return Hop3(3, address(0), tokenOut, 30_000, 300, 25_000);
    }

    function aeroHop(address tokenOut) internal pure returns (Hop3 memory) {
        return Hop3(2, AERO, tokenOut, 0, 0, 0);
    }

    /// v4 first buys NOCK there and sells it on Aerodrome, so NOCK must be
    /// dearer on Aerodrome: buy it there. Aerodrome first, the opposite. The
    /// trigger doubles until the route pays.
    function openGap(bool v4First, Hop3 memory a, Hop3 memory b) internal returns (Sized memory s) {
        uint256 usdcIn = 100e6;
        uint256 snap = vm.snapshotState();
        while (true) {
            if (v4First) {
                deal(USDC, address(trigger), usdcIn);
                trigger.buy(2, AERO, USDC, NOCK, usdcIn);
            } else {
                // About 61 NOCK a USDC: NOCK has 16 decimals, USDC 6.
                uint256 nockIn = usdcIn * 61e10;
                deal(NOCK, address(trigger), nockIn);
                trigger.buy(2, AERO, NOCK, USDC, nockIn);
            }
            s = sizer.size(a, b, USDC, CAP, PRICE, RATE, 8);
            if (s.size != 0 && s.out > s.size + 10_000) return s;
            require(usdcIn < 400_000e6, "no gap opened");
            vm.revertToState(snap);
            snap = vm.snapshotState();
            usdcIn *= 2;
        }
    }

    function trade(bool v4First) internal {
        (Hop3 memory a, Hop3 memory b) = v4First ? (v4Hop(NOCK), aeroHop(USDC)) : (aeroHop(NOCK), v4Hop(USDC));
        Sized memory s = openGap(v4First, a, b);
        bytes memory p = abi.encodePacked(uint8(2), uint8(1), CAP, uint40(block.timestamp + 10), RATE);
        p = abi.encodePacked(p, uint32(OVERHEAD), uint32(380_000), uint64(3e9), uint64(1), uint8(8));
        p = abi.encodePacked(p, a.kind, a.pool, a.tokenOut, a.fee, a.tickSpacing, a.crossGas);
        p = abi.encodePacked(p, b.kind, b.pool, b.tokenOut, b.fee, b.tickSpacing, b.crossGas);
        uint256 before = IERC20Fork3(USDC).balanceOf(recipient);
        vm.recordLogs();
        vm.txGasPrice(PRICE);
        lean.run(p);
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
        assertEq(IERC20Fork3(USDC).balanceOf(recipient) - before, s.out - s.size, "the gross, to the recipient");
        assertEq(IERC20Fork3(USDC).balanceOf(address(lean)), 0, "no USDC left");
        assertEq(IERC20Fork3(NOCK).balanceOf(address(lean)), 0, "no NOCK left");
        console.log("v4 first %s: size %s USDC units, gas %s", v4First, s.size, gas);
        console.log("  steps %s, gross %s", s.steps, s.out - s.size);
    }

    function testV4ThenAero() public {
        trade(true);
    }

    function testAeroThenV4() public {
        trade(false);
    }
}
