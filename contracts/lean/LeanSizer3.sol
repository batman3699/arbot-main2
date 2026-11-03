// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {FullMath} from "../libraries/FullMath.sol";
import {Bound, Leg, LeanPools3} from "./LeanPools3.sol";
import {LeanSwapMath3} from "./LeanSwapMath3.sol";

uint8 constant FLAG_CAPPED = 1;
uint8 constant FLAG_TRUNCATED = 2;

/// What the sizer found: the size, the output it quotes exactly, the gas of
/// the crossings the size takes, the boundaries reached, and the flags.
struct Sized {
    uint256 size;
    uint256 mid;
    uint256 out;
    uint256 crossGas;
    uint256 steps;
    uint8 flags;
}

/// A leg's running position in the walk. The leg itself keeps where it was
/// loaded, for the quote.
struct Pos {
    uint160 sqrtP;
    uint128 liquidity;
    uint256 reserveIn;
    uint256 reserveOut;
    /// The boundary the leg is heading for, in `leg.bounds`.
    uint256 k;
}

/// The size `LeanExecutor3` trades (Task 8.5 R40.1), and its exact output.
/// `apex_math::walk` is the same walk, integer for integer, and
/// `crates/apex-exec/tests/fixtures/sizer3_cases.json` holds the two to each
/// other.
///
/// Inside a pair of liquidity segments each pool is a constant product on
/// virtual reserves, so the best input has a closed form. When it would take
/// either pool past its next boundary, both move to the nearer boundary and
/// the next pair is evaluated. Its rounding only decides how good the size is:
/// `quote` then prices it step for step as the pools do, and the floor is
/// checked on what the swaps return.
library LeanSizer3 {
    uint256 internal constant MAX_STEPS_LIMIT = 16;
    uint256 internal constant Q96 = 0x1000000000000000000000000;
    uint256 internal constant PPM = 1_000_000;

    /// The size to trade, at most `cap`, after at most `maxSteps` boundaries,
    /// each crossing charged at `weiPerGas` in the start token at `rate` (0:
    /// one to one). Size 0: no gap.
    function size(Leg memory a, Leg memory b, uint256 cap, uint256 weiPerGas, uint256 rate, uint256 maxSteps)
        internal
        view
        returns (Sized memory r)
    {
        Pos memory pa = _start(a);
        Pos memory pb = _start(b);
        // The fees' root, scaled by 2^64: the fees do not change in a walk.
        uint256 keep = LeanSwapMath3.sqrt((_keep(a) * _keep(b)) << 128);
        if (_margin(a, pa, b, pb, keep) <= Q96) return r;
        uint256 total;
        while (true) {
            uint256 best = FullMath.mulDiv(_scale(a, pa, b, pb), _margin(a, pa, b, pb, keep) - Q96, Q96);
            bool clamped = total + best >= cap;
            uint256 dx = clamped ? cap - total : best;
            (uint256 inA, uint256 outA) = _toBoundary(a, pa);
            (uint256 inB,) = _toBoundary(b, pb);
            bool hitA = dx >= inA;
            uint256 reach = hitA ? outA : _forward(a, _copy(pa), dx);
            bool hitB = reach >= inB;
            if (!hitA && !hitB) {
                total += dx;
                if (clamped) r.flags |= FLAG_CAPPED;
                break;
            }
            bool aFirst = hitA && (!hitB || outA <= inB);
            if (aFirst) {
                _forward(b, pb, outA);
                total += inA;
            } else {
                uint256 back = _inputFor(a, pa, inB);
                _forward(a, pa, back);
                total += back;
            }
            r.steps++;
            if (r.steps >= maxSteps) {
                r.flags |= FLAG_TRUNCATED;
                if (total > 0) total -= 1;
                break;
            }
            uint256 gas = aFirst ? _cross(a, pa) : _cross(b, pb);
            uint256 cost = _toStart(gas * weiPerGas, rate);
            // No liquidity left in a segment has no gain either: this stops there too.
            if (_gain(a, pa, b, pb, keep) <= cost) {
                if (total > 0) total -= 1;
                break;
            }
            r.crossGas += gas;
            if (aFirst) _ensure(a, pa);
            else _ensure(b, pb);
        }
        r.size = total;
        if (total != 0) {
            r.mid = quote(a, total);
            r.out = quote(b, r.mid);
        }
    }

    /// What `amountIn` of the leg's input returns from where it was loaded,
    /// step for step as the pool would pay it.
    function quote(Leg memory leg, uint256 amountIn) internal view returns (uint256 out) {
        if (leg.kind == 2) {
            uint256 after_ = amountIn - FullMath.mulDiv(amountIn, leg.feePpm, PPM);
            return FullMath.mulDiv(after_, leg.reserveOut, leg.reserveIn + after_);
        }
        uint256 remaining = amountIn;
        uint160 sqrtP = leg.sqrtP;
        uint128 liquidity = leg.liquidity;
        uint256 k;
        while (remaining != 0) {
            if (k == leg.known) LeanPools3.nextBound(leg);
            Bound memory b = leg.bounds[k];
            (uint160 next, uint256 stepIn, uint256 stepOut, uint256 stepFee) =
                LeanSwapMath3.computeSwapStep(sqrtP, b.sqrtPrice, liquidity, remaining, leg.feePpm);
            remaining -= stepIn + stepFee;
            out += stepOut;
            sqrtP = next;
            // Landing exactly on a boundary pays the same crossed or not: not
            // crossing saves reading its net.
            if (sqrtP != b.sqrtPrice || remaining == 0) break;
            if (b.initialized) {
                liquidity = _applyNet(liquidity, LeanPools3.netOf(leg, k), leg.zeroForOne);
                if (liquidity == 0) revert();
            }
            k++;
        }
    }

    function _start(Leg memory leg) private view returns (Pos memory p) {
        p.sqrtP = leg.sqrtP;
        p.liquidity = leg.liquidity;
        p.reserveIn = leg.reserveIn;
        p.reserveOut = leg.reserveOut;
        if (leg.kind != 2 && leg.known == 0) LeanPools3.nextBound(leg);
    }

    function _copy(Pos memory p) private pure returns (Pos memory) {
        return Pos(p.sqrtP, p.liquidity, p.reserveIn, p.reserveOut, p.k);
    }

    function _ensure(Leg memory leg, Pos memory p) private view {
        if (p.k == leg.known) LeanPools3.nextBound(leg);
    }

    function _keep(Leg memory leg) private pure returns (uint256) {
        if (leg.feePpm >= PPM) revert();
        return PPM - leg.feePpm;
    }

    function _toStart(uint256 wei_, uint256 rate) private pure returns (uint256) {
        return rate == 0 ? wei_ : LeanSwapMath3.mulDivUp(wei_, rate, 1e18);
    }

    /// The constant product the leg sits on: what goes in, what comes out.
    function _virtuals(Leg memory leg, Pos memory p) private pure returns (uint256, uint256) {
        if (leg.kind == 2) return (p.reserveIn, p.reserveOut);
        uint256 zero = FullMath.mulDiv(p.liquidity, Q96, p.sqrtP);
        uint256 one = FullMath.mulDiv(p.liquidity, p.sqrtP, Q96);
        return leg.zeroForOne ? (zero, one) : (one, zero);
    }

    /// `sqrt(out / in)`, Q96.
    function _root(Leg memory leg, Pos memory p) private pure returns (uint256) {
        if (leg.kind == 2) return LeanSwapMath3.sqrt(FullMath.mulDiv(p.reserveOut, 1 << 192, p.reserveIn));
        return leg.zeroForOne ? p.sqrtP : FullMath.mulDiv(Q96, Q96, p.sqrtP);
    }

    /// The pair's `sqrt(a / b)`, Q96: above one in Q96, a trade pays. `keep`
    /// is the fees' root, scaled by 2^64: in whole ppm it would blur a small gap.
    function _margin(Leg memory a, Pos memory pa, Leg memory b, Pos memory pb, uint256 keep)
        private
        pure
        returns (uint256)
    {
        uint256 roots = FullMath.mulDiv(_root(a, pa), _root(b, pb), Q96);
        return FullMath.mulDiv(roots, keep, PPM << 64);
    }

    /// `b / c`: the best input is this times the margin less one.
    function _scale(Leg memory a, Pos memory pa, Leg memory b, Pos memory pb) private pure returns (uint256) {
        (uint256 aIn, uint256 aOut) = _virtuals(a, pa);
        (uint256 bIn,) = _virtuals(b, pb);
        return FullMath.mulDiv(aIn, bIn * PPM * PPM, _keep(a) * (bIn * PPM + _keep(b) * aOut));
    }

    /// The profit of the pair's closed-form best, boundaries ignored.
    function _gain(Leg memory a, Pos memory pa, Leg memory b, Pos memory pb, uint256 keep)
        private
        pure
        returns (uint256)
    {
        uint256 m = _margin(a, pa, b, pb, keep);
        if (m <= Q96) return 0;
        uint256 over = m - Q96;
        return FullMath.mulDiv(FullMath.mulDiv(_scale(a, pa, b, pb), over, Q96), over, Q96);
    }

    /// The gross input taking the leg to its boundary, and the output there.
    function _toBoundary(Leg memory leg, Pos memory p) private pure returns (uint256 gross, uint256 out) {
        if (leg.kind == 2) return (type(uint256).max, type(uint256).max);
        uint160 target = leg.bounds[p.k].sqrtPrice;
        uint256 netIn;
        if (leg.zeroForOne) {
            netIn = LeanSwapMath3.amount0Delta(target, p.sqrtP, p.liquidity, true);
            out = LeanSwapMath3.amount1Delta(target, p.sqrtP, p.liquidity, false);
        } else {
            netIn = LeanSwapMath3.amount1Delta(p.sqrtP, target, p.liquidity, true);
            out = LeanSwapMath3.amount0Delta(p.sqrtP, target, p.liquidity, false);
        }
        gross = LeanSwapMath3.mulDivUp(netIn, PPM, _keep(leg));
    }

    /// Move forward by `amount` in, inside the segment. Returns the output.
    function _forward(Leg memory leg, Pos memory p, uint256 amount) private pure returns (uint256 out) {
        if (amount == 0) return 0;
        if (leg.kind == 2) {
            uint256 after_ = amount - FullMath.mulDiv(amount, leg.feePpm, PPM);
            out = FullMath.mulDiv(after_, p.reserveOut, p.reserveIn + after_);
            p.reserveIn += after_;
            p.reserveOut -= out;
            return out;
        }
        uint160 next;
        (next,, out,) =
            LeanSwapMath3.computeSwapStep(p.sqrtP, leg.bounds[p.k].sqrtPrice, p.liquidity, amount, leg.feePpm);
        p.sqrtP = next;
    }

    /// The gross input that returns `out` from the segment, rounded up.
    function _inputFor(Leg memory leg, Pos memory p, uint256 out) private pure returns (uint256) {
        (uint256 vIn, uint256 vOut) = _virtuals(leg, p);
        uint256 net = LeanSwapMath3.mulDivUp(out, vIn, vOut - out);
        return LeanSwapMath3.mulDivUp(net, PPM, _keep(leg));
    }

    /// Step onto the boundary, and past it. Returns the crossing's gas, 0 at
    /// a word's edge.
    function _cross(Leg memory leg, Pos memory p) private view returns (uint256 gas) {
        Bound memory b = leg.bounds[p.k];
        p.sqrtP = b.sqrtPrice;
        p.k++;
        if (!b.initialized) return 0;
        p.liquidity = _applyNet(p.liquidity, LeanPools3.netOf(leg, p.k - 1), leg.zeroForOne);
        return leg.crossGas;
    }

    function _applyNet(uint128 liquidity, int128 net, bool zeroForOne) private pure returns (uint128) {
        int256 signed = zeroForOne ? -int256(net) : int256(net);
        return signed >= 0 ? liquidity + uint128(uint256(signed)) : liquidity - uint128(uint256(-signed));
    }
}
