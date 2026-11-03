// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Hop3} from "./LeanPlan3Codec.sol";
import {TickMath} from "../libraries/TickMath.sol";

/// One boundary a leg's walk can reach: the next initialized tick, or a bitmap
/// word's edge (`initialized` false).
struct Bound {
    int24 tick;
    bool initialized;
    uint160 sqrtPrice;
    int128 net;
    bool netRead;
}

/// A pool as `LeanExecutor3` loaded it at execution, and the boundaries found
/// so far, in the order a swap meets them.
struct Leg {
    uint8 kind;
    address pool;
    bool zeroForOne;
    /// v4 only: the pool's state slot in PoolManager.
    bytes32 state;
    uint160 sqrtP;
    uint128 liquidity;
    int24 tick;
    int24 spacing;
    uint24 feePpm;
    /// Aerodrome v2 only.
    uint256 reserveIn;
    uint256 reserveOut;
    /// The one bitmap word held.
    int16 wordPos;
    uint256 word;
    bool wordRead;
    Bound[18] bounds;
    uint256 known;
    uint16 crossGas;
}

interface IClViews3 {
    function liquidity() external view returns (uint128);
    function fee() external view returns (uint24);
    function tickBitmap(int16 wordPosition) external view returns (uint256);
}

interface IExtsload3 {
    function extsload(bytes32 slot) external view returns (bytes32);
}

interface IAeroPool3 {
    function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
    function factory() external view returns (address);
}

interface IAeroFactory3 {
    function getFee(address pool, bool stable) external view returns (uint256);
}

/// Reading a pool at execution (Task 8.5 R40.1), each venue its own way:
/// Uniswap v3, PancakeSwap v3 and Slipstream through their views, Uniswap v4
/// through `extsload` on PoolManager, and Aerodrome v2 through its reserves.
///
/// `slot0` and `ticks` are read raw, their first two words only: past those
/// the venues' return types differ (Slipstream's `slot0` ends in a `bool` where
/// Uniswap's has a fee), and a typed call reverts on the others.
library LeanPools3 {
    address internal constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;
    uint256 internal constant MAX_BOUNDS = 18;
    /// A v4 hook that returns a swap delta changes what the swap pays.
    uint160 internal constant DELTA_HOOK_BITS = (1 << 3) | (1 << 2);
    /// A v4 pool key whose fee is this is dynamic.
    uint24 internal constant DYNAMIC_FEE = 0x800000;

    /// `leg` loaded from `hop`, trading `tokenIn`. False: a pool the sizer
    /// must not trade, which the executor reports as `NoTrade(UNSUPPORTED)`.
    function load(Hop3 memory hop, address tokenIn, Leg memory leg) internal view returns (bool) {
        leg.kind = hop.kind;
        leg.pool = hop.pool;
        leg.zeroForOne = tokenIn < hop.tokenOut;
        leg.spacing = hop.tickSpacing;
        leg.crossGas = hop.crossGas;
        if (hop.kind == 2) {
            (uint256 r0, uint256 r1,) = IAeroPool3(hop.pool).getReserves();
            (leg.reserveIn, leg.reserveOut) = leg.zeroForOne ? (r0, r1) : (r1, r0);
            leg.feePpm = uint24(IAeroFactory3(IAeroPool3(hop.pool).factory()).getFee(hop.pool, false) * 100);
            return r0 != 0 && r1 != 0;
        }
        if (hop.kind == 3) {
            if (uint160(hop.pool) & DELTA_HOOK_BITS != 0 || hop.fee & DYNAMIC_FEE != 0) return false;
            (address c0, address c1) = leg.zeroForOne ? (tokenIn, hop.tokenOut) : (hop.tokenOut, tokenIn);
            bytes32 id = keccak256(abi.encode(c0, c1, hop.fee, hop.tickSpacing, hop.pool));
            leg.state = keccak256(abi.encode(id, uint256(6)));
            uint256 word = uint256(IExtsload3(POOL_MANAGER).extsload(leg.state));
            leg.sqrtP = uint160(word);
            leg.tick = int24(uint24(word >> 160));
            if (uint24(word >> 184) != 0) return false;
            leg.feePpm = uint24(word >> 208);
            leg.liquidity = uint128(uint256(IExtsload3(POOL_MANAGER).extsload(bytes32(uint256(leg.state) + 3))));
        } else {
            (uint256 sqrtWord, uint256 tickWord) = _twoWords(hop.pool, abi.encodeWithSignature("slot0()"));
            leg.sqrtP = uint160(sqrtWord);
            leg.tick = int24(int256(tickWord));
            leg.liquidity = IClViews3(hop.pool).liquidity();
            leg.feePpm = hop.kind == 4 ? IClViews3(hop.pool).fee() : hop.fee;
        }
        return leg.liquidity != 0 && leg.sqrtP != 0;
    }

    /// Append the boundary after the last one found: v3's
    /// `nextInitializedTickWithinOneWord` from where that one leaves the swap.
    function nextBound(Leg memory leg) internal view {
        int24 origin = leg.tick;
        if (leg.known != 0) {
            int24 last = leg.bounds[leg.known - 1].tick;
            origin = leg.zeroForOne ? last - 1 : last;
        }
        int24 s = leg.spacing;
        int24 compressed = origin / s;
        if (origin < 0 && origin % s != 0) compressed--;
        int24 next;
        bool initialized;
        if (leg.zeroForOne) {
            (int16 wordPos, uint8 bitPos) = _position(compressed);
            uint256 masked = _word(leg, wordPos) & ((1 << bitPos) - 1 + (1 << bitPos));
            initialized = masked != 0;
            next = initialized
                ? (compressed - int24(uint24(bitPos - _msb(masked)))) * s
                : (compressed - int24(uint24(bitPos))) * s;
        } else {
            (int16 wordPos, uint8 bitPos) = _position(compressed + 1);
            uint256 masked = _word(leg, wordPos) & ~((1 << bitPos) - 1);
            initialized = masked != 0;
            next = initialized
                ? (compressed + 1 + int24(uint24(_lsb(masked) - bitPos))) * s
                : (compressed + 1 + int24(uint24(type(uint8).max - bitPos))) * s;
        }
        if (next < TickMath.MIN_TICK) next = TickMath.MIN_TICK;
        if (next > TickMath.MAX_TICK) next = TickMath.MAX_TICK;
        if (leg.known == MAX_BOUNDS) revert();
        Bound memory b = leg.bounds[leg.known];
        b.tick = next;
        b.initialized = initialized;
        b.sqrtPrice = TickMath.getSqrtRatioAtTick(next);
        leg.known++;
    }

    /// The liquidity net at `bounds[i]`, read once.
    function netOf(Leg memory leg, uint256 i) internal view returns (int128) {
        Bound memory b = leg.bounds[i];
        if (!b.initialized) return 0;
        if (!b.netRead) {
            if (leg.kind == 3) {
                bytes32 slot = keccak256(abi.encode(int256(b.tick), uint256(leg.state) + 4));
                b.net = int128(int256(uint256(IExtsload3(POOL_MANAGER).extsload(slot)) >> 128));
            } else {
                (, uint256 netWord) = _twoWords(leg.pool, abi.encodeWithSignature("ticks(int24)", b.tick));
                b.net = int128(int256(netWord));
            }
            b.netRead = true;
        }
        return b.net;
    }

    function _word(Leg memory leg, int16 wordPos) private view returns (uint256) {
        if (!leg.wordRead || leg.wordPos != wordPos) {
            if (leg.kind == 3) {
                bytes32 slot = keccak256(abi.encode(int256(wordPos), uint256(leg.state) + 5));
                leg.word = uint256(IExtsload3(POOL_MANAGER).extsload(slot));
            } else {
                leg.word = IClViews3(leg.pool).tickBitmap(wordPos);
            }
            leg.wordPos = wordPos;
            leg.wordRead = true;
        }
        return leg.word;
    }

    function _position(int24 compressed) private pure returns (int16 wordPos, uint8 bitPos) {
        wordPos = int16(compressed >> 8);
        bitPos = uint8(uint24(compressed % 256));
    }

    /// The first two words a view returns, whatever follows them.
    function _twoWords(address target, bytes memory data) private view returns (uint256 a, uint256 b) {
        bool ok;
        // Only the scratch space is written.
        assembly ("memory-safe") {
            ok := staticcall(gas(), target, add(data, 0x20), mload(data), 0, 0x40)
            a := mload(0)
            b := mload(0x20)
        }
        if (!ok) revert();
        uint256 size;
        assembly ("memory-safe") {
            size := returndatasize()
        }
        if (size < 0x40) revert();
    }

    function _msb(uint256 x) private pure returns (uint8 r) {
        if (x >= 0x100000000000000000000000000000000) {
            x >>= 128;
            r += 128;
        }
        if (x >= 0x10000000000000000) {
            x >>= 64;
            r += 64;
        }
        if (x >= 0x100000000) {
            x >>= 32;
            r += 32;
        }
        if (x >= 0x10000) {
            x >>= 16;
            r += 16;
        }
        if (x >= 0x100) {
            x >>= 8;
            r += 8;
        }
        if (x >= 0x10) {
            x >>= 4;
            r += 4;
        }
        if (x >= 0x4) {
            x >>= 2;
            r += 2;
        }
        if (x >= 0x2) r += 1;
    }

    function _lsb(uint256 x) private pure returns (uint8) {
        return _msb(x & (~x + 1));
    }
}
