// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {LeanToken, pay} from "./LeanMocks.sol";
import {LeanSwapMath3} from "../../contracts/lean/LeanSwapMath3.sol";
import {TickMath} from "../../contracts/libraries/TickMath.sol";

/// Test doubles for the third lean executor (Task 8.5 R40.1). Unlike R29's,
/// these price as the venues do: a concentrated pool walks its own bitmap and
/// ticks with v3's swap loop, and an Aerodrome pool pays `getAmountOut`. So the
/// sizer's answer can be checked against what a swap actually returns.

interface IV3Callback3 {
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external;
    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external;
}

/// v3's swap loop over pools held by id, exact input only. v3, Pancake and
/// Slipstream pools hold one (id 0); PoolManager holds many.
abstract contract MockClEngine {
    struct P {
        uint160 sqrtP;
        int24 tick;
        uint128 liquidity;
        uint24 fee;
        int24 spacing;
    }

    mapping(bytes32 => P) internal pools;
    mapping(bytes32 => mapping(int16 => uint256)) internal bitmaps;
    mapping(bytes32 => mapping(int24 => int128)) internal nets;
    /// Initialized ticks crossed by the last swap.
    uint256 public lastCrossings;

    function _init(bytes32 id, int24 tick, uint128 liquidity, uint24 fee, int24 spacing) internal {
        pools[id] = P(TickMath.getSqrtRatioAtTick(tick), tick, liquidity, fee, spacing);
        _changed(id);
    }

    /// Initialize `tick` with `net`. A tick set twice keeps the second net.
    function _setTick(bytes32 id, int24 tick, int128 net) internal {
        int24 s = pools[id].spacing;
        require(tick % s == 0, "unspaced");
        int24 compressed = tick / s;
        bitmaps[id][int16(compressed >> 8)] |= 1 << uint8(uint24(compressed % 256));
        nets[id][tick] = net;
        _tickChanged(id, tick, int16(compressed >> 8));
    }

    function _next(bytes32 id, int24 tick, bool lte) internal view returns (int24 next, bool initialized) {
        int24 s = pools[id].spacing;
        int24 compressed = tick / s;
        if (tick < 0 && tick % s != 0) compressed--;
        if (lte) {
            int16 wordPos = int16(compressed >> 8);
            uint8 bitPos = uint8(uint24(compressed % 256));
            uint256 masked = bitmaps[id][wordPos] & ((1 << bitPos) - 1 + (1 << bitPos));
            initialized = masked != 0;
            next = initialized ? (compressed - int24(uint24(bitPos - msb(masked)))) * s : (compressed - int24(uint24(bitPos))) * s;
        } else {
            int16 wordPos = int16((compressed + 1) >> 8);
            uint8 bitPos = uint8(uint24((compressed + 1) % 256));
            uint256 masked = bitmaps[id][wordPos] & ~((1 << bitPos) - 1);
            initialized = masked != 0;
            next = initialized
                ? (compressed + 1 + int24(uint24(msb(masked & (~masked + 1)) - bitPos))) * s
                : (compressed + 1 + int24(uint24(type(uint8).max - bitPos))) * s;
        }
    }

    /// v3's swap: step to each boundary, cross the initialized ones (eagerly,
    /// as v3 does when the input runs out exactly there), stop at the input's
    /// end or the price limit.
    function _swap(bytes32 id, bool zeroForOne, uint256 amountIn, uint160 limit) internal returns (uint256 used, uint256 out) {
        P memory p = pools[id];
        uint256 remaining = amountIn;
        lastCrossings = 0;
        while (remaining != 0 && p.sqrtP != limit) {
            (int24 next, bool initialized) = _next(id, p.tick, zeroForOne);
            if (next < TickMath.MIN_TICK) next = TickMath.MIN_TICK;
            if (next > TickMath.MAX_TICK) next = TickMath.MAX_TICK;
            uint160 nextSqrt = TickMath.getSqrtRatioAtTick(next);
            uint160 target = zeroForOne ? (nextSqrt < limit ? limit : nextSqrt) : (nextSqrt > limit ? limit : nextSqrt);
            uint160 sqrtBefore = p.sqrtP;
            (uint160 after_, uint256 stepIn, uint256 stepOut, uint256 stepFee) =
                LeanSwapMath3.computeSwapStep(p.sqrtP, target, p.liquidity, remaining, p.fee);
            p.sqrtP = after_;
            remaining -= stepIn + stepFee;
            out += stepOut;
            if (p.sqrtP == nextSqrt) {
                if (initialized) {
                    int128 net = nets[id][next];
                    if (zeroForOne) net = -net;
                    p.liquidity = net < 0 ? p.liquidity - uint128(-net) : p.liquidity + uint128(net);
                    lastCrossings++;
                }
                p.tick = zeroForOne ? next - 1 : next;
            } else if (p.sqrtP != sqrtBefore) {
                p.tick = tickAt(p.sqrtP);
            }
        }
        used = amountIn - remaining;
        pools[id] = p;
        _changed(id);
    }

    /// The greatest tick whose price is at most `sqrtP`, by search.
    function tickAt(uint160 sqrtP) internal pure returns (int24) {
        int24 lo = TickMath.MIN_TICK;
        int24 hi = TickMath.MAX_TICK;
        while (lo < hi) {
            int24 mid = int24((int256(lo) + int256(hi) + 1) >> 1);
            if (TickMath.getSqrtRatioAtTick(mid) <= sqrtP) lo = mid;
            else hi = mid - 1;
        }
        return lo;
    }

    function msb(uint256 x) internal pure returns (uint8 r) {
        while (x > 1) {
            x >>= 1;
            r++;
        }
    }

    function _changed(bytes32 id) internal virtual {}

    function _tickChanged(bytes32 id, int24 tick, int16 wordPos) internal virtual {}
}

/// A Uniswap v3, PancakeSwap v3 or Slipstream pool. Their `slot0` and `ticks`
/// share their first two words and differ after, as the real ones do.
contract MockCl3Pool is MockClEngine {
    uint8 public constant UNISWAP = 0;
    uint8 public constant PANCAKE = 1;
    uint8 public constant SLIPSTREAM = 2;

    address public immutable token0;
    address public immutable token1;
    uint8 public immutable shape;
    /// When set, the pool pays out and never calls back.
    bool public skipCallback;
    uint256 public swaps;

    constructor(address a, address b, uint8 shape_, int24 tick, uint128 liquidity, uint24 fee, int24 spacing) {
        (token0, token1) = a < b ? (a, b) : (b, a);
        shape = shape_;
        _init(0, tick, liquidity, fee, spacing);
    }

    function setTick(int24 tick, int128 net) external {
        _setTick(0, tick, net);
    }

    /// Slipstream's fee moves with its TWAP; this one moves when told.
    function setFee(uint24 fee_) external {
        pools[0].fee = fee_;
    }

    function setPrice(int24 tick) external {
        pools[0].tick = tick;
        pools[0].sqrtP = TickMath.getSqrtRatioAtTick(tick);
    }

    function setSkipCallback(bool s) external {
        skipCallback = s;
    }

    function sqrtPriceX96() external view returns (uint160) {
        return pools[0].sqrtP;
    }

    function slot0() external view returns (bytes memory) {
        P memory p = pools[0];
        bytes memory words = shape == SLIPSTREAM
            ? abi.encode(p.sqrtP, p.tick, uint16(0), uint16(1), uint16(1), true)
            : shape == PANCAKE
                ? abi.encode(p.sqrtP, p.tick, uint16(0), uint16(1), uint16(1), uint32(0x00960096), true)
                : abi.encode(p.sqrtP, p.tick, uint16(0), uint16(1), uint16(1), uint8(0x44), true);
        assembly {
            return(add(words, 0x20), mload(words))
        }
    }

    function liquidity() external view returns (uint128) {
        return pools[0].liquidity;
    }

    function fee() external view returns (uint24) {
        return pools[0].fee;
    }

    function tickSpacing() external view returns (int24) {
        return pools[0].spacing;
    }

    function tickBitmap(int16 wordPos) external view returns (uint256) {
        return bitmaps[0][wordPos];
    }

    function ticks(int24 tick) external view returns (bytes memory) {
        int128 net = nets[0][tick];
        uint128 gross = uint128(net < 0 ? -net : net);
        bytes memory words = shape == SLIPSTREAM
            ? abi.encode(gross, net, int128(0), uint256(0), uint256(0), uint256(0), int56(0), uint160(0), uint32(0), gross != 0)
            : abi.encode(gross, net, uint256(0), uint256(0), int56(0), uint160(0), uint32(0), gross != 0);
        assembly {
            return(add(words, 0x20), mload(words))
        }
    }

    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 limit, bytes calldata)
        external
        returns (int256 amount0, int256 amount1)
    {
        require(amountSpecified > 0, "exact input only");
        swaps++;
        (uint256 used, uint256 out) = _swap(0, zeroForOne, uint256(amountSpecified), limit);
        (address tokenIn, address tokenOut) = zeroForOne ? (token0, token1) : (token1, token0);
        pay(tokenOut, recipient, out);
        (amount0, amount1) = zeroForOne ? (int256(used), -int256(out)) : (-int256(out), int256(used));
        if (skipCallback) return (amount0, amount1);
        uint256 before = LeanToken(tokenIn).balanceOf(address(this));
        if (shape == PANCAKE) {
            IV3Callback3(msg.sender).pancakeV3SwapCallback(amount0, amount1, "");
        } else {
            IV3Callback3(msg.sender).uniswapV3SwapCallback(amount0, amount1, "");
        }
        require(LeanToken(tokenIn).balanceOf(address(this)) >= before + used, "IIA");
    }
}

contract MockAeroFactory3 {
    mapping(address => uint256) public feeOf;

    function setFee(address pool, uint256 bps) external {
        feeOf[pool] = bps;
    }

    function getFee(address pool, bool stable) external view returns (uint256) {
        require(!stable, "volatile only");
        return feeOf[pool];
    }
}

/// An Aerodrome v2 volatile pool: paid first, then pays what it is asked if
/// the product, its fee taken off the input, does not fall.
contract MockAero3Pool {
    address public immutable token0;
    address public immutable token1;
    address public immutable factory;
    uint256 public reserve0;
    uint256 public reserve1;

    constructor(address a, address b, MockAeroFactory3 factory_, uint256 r0, uint256 r1) {
        (token0, token1) = a < b ? (a, b) : (b, a);
        factory = address(factory_);
        (reserve0, reserve1) = (r0, r1);
    }

    function getReserves() external view returns (uint256, uint256, uint256) {
        return (reserve0, reserve1, block.timestamp);
    }

    /// `Pool.getAmountOut`.
    function getAmountOut(uint256 amountIn, address tokenIn) public view returns (uint256) {
        (uint256 rIn, uint256 rOut) = tokenIn == token0 ? (reserve0, reserve1) : (reserve1, reserve0);
        amountIn -= amountIn * MockAeroFactory3(factory).feeOf(address(this)) / 10_000;
        return amountIn * rOut / (rIn + amountIn);
    }

    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata) external {
        require(amount0Out == 0 || amount1Out == 0, "one side out");
        if (amount0Out > 0) pay(token0, to, amount0Out);
        if (amount1Out > 0) pay(token1, to, amount1Out);
        uint256 b0 = LeanToken(token0).balanceOf(address(this));
        uint256 b1 = LeanToken(token1).balanceOf(address(this));
        uint256 in0 = b0 > reserve0 - amount0Out ? b0 - (reserve0 - amount0Out) : 0;
        uint256 in1 = b1 > reserve1 - amount1Out ? b1 - (reserve1 - amount1Out) : 0;
        uint256 bps = MockAeroFactory3(factory).feeOf(address(this));
        uint256 after0 = b0 - in0 * bps / 10_000;
        uint256 after1 = b1 - in1 * bps / 10_000;
        require(after0 * after1 >= reserve0 * reserve1, "K");
        (reserve0, reserve1) = (after0, after1);
    }
}

struct MockPoolKey3 {
    address currency0;
    address currency1;
    uint24 fee;
    int24 tickSpacing;
    address hooks;
}

struct MockSwapParams3 {
    bool zeroForOne;
    int256 amountSpecified;
    uint160 sqrtPriceLimitX96;
}

interface IUnlockCallback3 {
    function unlockCallback(bytes calldata data) external returns (bytes memory);
}

/// Uniswap v4's PoolManager: flash accounting as R37's double has it, pools
/// priced by v3's loop, and their storage served through `extsload` at the
/// slots `StateLibrary` reads.
contract MockPoolManager3 is MockClEngine {
    error CurrencyNotSettled();
    error ManagerLocked();

    bool public unlocked;
    mapping(address => int256) public delta;
    uint256 public nonzeroDeltas;
    address public synced;
    uint256 public syncedReserves;
    uint256 public swaps;
    /// A swap that reports its input as owed to the caller: a delta of the
    /// wrong sign, which the executor must refuse.
    bool public flipDelta;
    mapping(bytes32 => bytes32) internal words;
    mapping(bytes32 => bytes32) internal stateOf;
    mapping(bytes32 => uint24) internal protocolFee;

    function idOf(MockPoolKey3 memory key) public pure returns (bytes32) {
        return keccak256(abi.encode(key.currency0, key.currency1, key.fee, key.tickSpacing, key.hooks));
    }

    function initialize(MockPoolKey3 memory key, int24 tick, uint128 liquidity) external {
        bytes32 id = idOf(key);
        stateOf[id] = keccak256(abi.encode(id, uint256(6)));
        _init(id, tick, liquidity, key.fee, key.tickSpacing);
    }

    function setTick(MockPoolKey3 memory key, int24 tick, int128 net) external {
        _setTick(idOf(key), tick, net);
    }

    function setFlipDelta(bool f) external {
        flipDelta = f;
    }

    function setProtocolFee(MockPoolKey3 memory key, uint24 f) external {
        protocolFee[idOf(key)] = f;
        _changed(idOf(key));
    }

    function sqrtPriceOf(MockPoolKey3 memory key) external view returns (uint160) {
        return pools[idOf(key)].sqrtP;
    }

    function extsload(bytes32 slot) external view returns (bytes32) {
        return words[slot];
    }

    function _changed(bytes32 id) internal override {
        P memory p = pools[id];
        bytes32 state = stateOf[id];
        uint256 word = uint256(p.sqrtP) | (uint256(uint24(p.tick)) << 160) | (uint256(protocolFee[id]) << 184)
            | (uint256(p.fee) << 208);
        words[state] = bytes32(word);
        words[bytes32(uint256(state) + 3)] = bytes32(uint256(p.liquidity));
    }

    function _tickChanged(bytes32 id, int24 tick, int16 wordPos) internal override {
        bytes32 state = stateOf[id];
        words[keccak256(abi.encode(int256(wordPos), uint256(state) + 5))] = bytes32(bitmaps[id][wordPos]);
        int128 net = nets[id][tick];
        uint128 gross = uint128(net < 0 ? -net : net);
        words[keccak256(abi.encode(int256(tick), uint256(state) + 4))] =
            bytes32((uint256(uint128(net)) << 128) | uint256(gross));
    }

    function unlock(bytes calldata data) external returns (bytes memory result) {
        if (unlocked) revert ManagerLocked();
        unlocked = true;
        result = IUnlockCallback3(msg.sender).unlockCallback(data);
        if (nonzeroDeltas != 0) revert CurrencyNotSettled();
        unlocked = false;
    }

    function swap(MockPoolKey3 memory key, MockSwapParams3 memory params, bytes calldata) external returns (int256) {
        if (!unlocked) revert ManagerLocked();
        require(key.currency0 < key.currency1, "unsorted key");
        require(params.amountSpecified < 0, "exact input only");
        swaps++;
        (uint256 used, uint256 out) =
            _swap(idOf(key), params.zeroForOne, uint256(-params.amountSpecified), params.sqrtPriceLimitX96);
        (address tokenIn, address tokenOut) =
            params.zeroForOne ? (key.currency0, key.currency1) : (key.currency1, key.currency0);
        _account(tokenIn, -int128(int256(used)));
        _account(tokenOut, int128(int256(out)));
        int128 owedIn = flipDelta ? int128(int256(used)) : -int128(int256(used));
        (int128 a0, int128 a1) = params.zeroForOne ? (owedIn, int128(int256(out))) : (int128(int256(out)), owedIn);
        return (int256(a0) << 128) | int256(uint256(uint128(a1)));
    }

    function sync(address currency) external {
        synced = currency;
        syncedReserves = LeanToken(currency).balanceOf(address(this));
    }

    function settle() external payable returns (uint256 paid) {
        if (!unlocked) revert ManagerLocked();
        paid = LeanToken(synced).balanceOf(address(this)) - syncedReserves;
        _account(synced, int128(int256(paid)));
        synced = address(0);
    }

    function take(address currency, address to, uint256 amount) external {
        if (!unlocked) revert ManagerLocked();
        _account(currency, -int128(int256(amount)));
        pay(currency, to, amount);
    }

    function _account(address currency, int128 d) private {
        if (d == 0) return;
        int256 before = delta[currency];
        int256 next = before + d;
        if (before == 0) nonzeroDeltas++;
        if (next == 0) nonzeroDeltas--;
        delta[currency] = next;
    }
}
