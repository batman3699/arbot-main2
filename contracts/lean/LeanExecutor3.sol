// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {BadPlan} from "./LeanPlanCodec.sol";
import {Hop3, LeanPlan3Codec, Plan3} from "./LeanPlan3Codec.sol";
import {Leg, LeanPools3} from "./LeanPools3.sol";
import {LeanSizer3, Sized} from "./LeanSizer3.sol";
import {LeanSwapMath3} from "./LeanSwapMath3.sol";

interface IERC20Min3 {
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

/// Uniswap v3, both Slipstream factories and PancakeSwap v3 share it.
interface IV3Pool3 {
    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data)
        external
        returns (int256 amount0, int256 amount1);
}

interface IAeroSwap3 {
    function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

interface IMorphoFlashLoan3 {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

interface IBalancerVault3 {
    function flashLoan(address recipient, address[] memory tokens, uint256[] memory amounts, bytes memory userData)
        external;
}

/// Uniswap v4's PoolKey and SwapParams, as PoolManager takes them.
struct PoolKey3 {
    address currency0;
    address currency1;
    uint24 fee;
    int24 tickSpacing;
    address hooks;
}

struct SwapParams3 {
    bool zeroForOne;
    int256 amountSpecified;
    uint160 sqrtPriceLimitX96;
}

/// The five PoolManager calls a trade makes, and nothing else.
interface IPoolManager3 {
    function unlock(bytes calldata data) external returns (bytes memory);
    function swap(PoolKey3 memory key, SwapParams3 memory params, bytes calldata hookData) external returns (int256);
    function sync(address currency) external;
    function settle() external payable returns (uint256);
    function take(address currency, address to, uint256 amount) external;
}

/// The third lean executor (Task 8.5 R40.1): it sizes the trade itself, at
/// execution. A plan names two pools and a cap, and no size or hop minimums.
/// `run` reads both pools, walks them to the best input (`LeanSizer3`), quotes
/// that input exactly, and either trades it or returns before borrowing,
/// reporting why.
///
/// The floor is priced at execution: the gas used so far and to come, at the
/// transaction's own gas price, with the plan's L1 bound, in the start token.
/// So a trade it lets through nets above zero after its gas, bid and L1 fee,
/// unless the L1 bound is exceeded, which the receipt shows.
///
/// Gas is measured a frame at a time: `run`'s own work before it borrows, and
/// the lender's callback's from its own entry. One `gasleft()` against the
/// start would also count the gas the 63/64 rule holds back in each calling
/// frame, which is reserved, not spent: millions of gas under a test's limit,
/// tens of thousands under a trade's. The plan's `gasOverhead` covers the rest:
/// the intrinsic gas and calldata, the lender's own work, and what follows the
/// floor.
///
/// Kept from v2: trader-only access, the owner's circuit breaker, the deadline,
/// the lenders and their callbacks. Not upgradeable.
contract LeanExecutor3 {
    address public constant WETH = 0x4200000000000000000000000000000000000006;
    address public constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address public constant BALANCER_VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    address public constant MORPHO_BLUE = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    address public constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;

    /// Why `run` traded nothing, in `NoTrade`.
    uint8 public constant NO_GAP = 0;
    uint8 public constant BELOW_FLOOR = 1;
    uint8 public constant UNSUPPORTED = 2;
    uint8 public constant LENDER_SHORT = 3;

    /// No price limit: the floor is the bound (`MIN_SQRT_RATIO + 1`,
    /// `MAX_SQRT_RATIO - 1`; v4's bounds are v3's).
    uint160 private constant MIN_LIMIT = 4295128740;
    uint160 private constant MAX_LIMIT = 1461446703485210103287273052203988822378723970341;

    /// Transient slots (EIP-1153): live only inside one transaction.
    /// The loan `run` started and its lender has not yet called back.
    uint256 private constant T_LOAN = 0;
    /// The pool being swapped with, until its callback pays it.
    uint256 private constant T_POOL = 1;
    /// What that pool is paid in.
    uint256 private constant T_TOKEN_IN = 2;
    /// The size `run` chose, the gas `run` used before it borrowed, and its
    /// caller with the walk's steps and flags.
    uint256 private constant T_SIZE = 3;
    uint256 private constant T_GAS = 4;
    uint256 private constant T_CALLER = 5;

    address public owner;
    mapping(address => bool) public executors;
    /// Read with the circuit on every trade: one slot, one cold read.
    address public profitRecipient;
    bool private circuitOpen;

    /// A trade: what was borrowed and what came back, both in the start token,
    /// the lender's fee, the cost the floor charged in wei, and the walk's
    /// boundaries and flags (1 held at the cap, 2 at the step limit).
    event Traded(
        address indexed sender,
        uint8 start,
        uint256 amountIn,
        uint256 out,
        uint256 lenderFee,
        uint256 costWei,
        uint8 steps,
        uint8 flags
    );
    /// Nothing traded, nothing borrowed.
    event NoTrade(address indexed sender, uint8 reason, uint8 steps);

    error NotOwner();
    error ZeroAddress();
    error NotTrader();
    error CircuitOpen();
    error Expired(uint256 deadline, uint256 timestamp);
    /// A lender, unlock or swap callback this executor did not ask for.
    error UnexpectedCallback();
    error Floor(uint256 out, uint256 required);
    error TransferFailed();
    /// A v4 swap whose deltas are not an input owed and an output credited.
    error V4Delta();

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    constructor(address owner_, address trader_, address profitRecipient_) {
        if (owner_ == address(0) || trader_ == address(0) || profitRecipient_ == address(0)) revert ZeroAddress();
        owner = owner_;
        executors[trader_] = true;
        profitRecipient = profitRecipient_;
        // Morpho Blue pulls its flash loan's repayment, in either start token.
        IERC20Min3(WETH).approve(MORPHO_BLUE, type(uint256).max);
        IERC20Min3(USDC).approve(MORPHO_BLUE, type(uint256).max);
    }

    /// The owner's stop, read by the runtime and by every trade.
    function isCircuitOpen() external view returns (bool) {
        return circuitOpen;
    }

    function setExecutor(address account, bool allowed) external onlyOwner {
        if (account == address(0)) revert ZeroAddress();
        executors[account] = allowed;
    }

    /// Every sender at once: one owner transaction for the whole set.
    function setExecutors(address[] calldata accounts, bool allowed) external onlyOwner {
        for (uint256 i; i < accounts.length; ++i) {
            if (accounts[i] == address(0)) revert ZeroAddress();
            executors[accounts[i]] = allowed;
        }
    }

    function setProfitRecipient(address recipient) external onlyOwner {
        if (recipient == address(0)) revert ZeroAddress();
        profitRecipient = recipient;
    }

    function tripCircuit() external onlyOwner {
        circuitOpen = true;
    }

    function resetCircuit() external onlyOwner {
        circuitOpen = false;
    }

    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        owner = newOwner;
    }

    function sweep(address token, address to, uint256 amount) external onlyOwner {
        _transfer(token, to, amount);
    }

    /// One trade, sized here. The cheap checks first, then the pools, then
    /// the size and its exact quote, then the predicted floor and the lender:
    /// any of those failing returns before anything is borrowed.
    function run(bytes calldata plan) external {
        uint256 gasStart = gasleft();
        if (!executors[msg.sender]) revert NotTrader();
        if (circuitOpen) revert CircuitOpen();
        Plan3 memory p = LeanPlan3Codec.decode(plan);
        if (block.timestamp > p.deadline) revert Expired(p.deadline, block.timestamp);
        address token = _startToken(p.start);
        if (p.hops[1].tokenOut != token) revert BadPlan();

        Leg memory a;
        Leg memory b;
        if (!LeanPools3.load(p.hops[0], token, a) || !LeanPools3.load(p.hops[1], p.hops[0].tokenOut, b)) {
            emit NoTrade(msg.sender, UNSUPPORTED, 0);
            return;
        }
        Sized memory s = LeanSizer3.size(a, b, p.cap, tx.gasprice, p.rate, p.maxSteps);
        if (s.size == 0) {
            emit NoTrade(msg.sender, NO_GAP, uint8(s.steps));
            return;
        }
        uint256 gasAhead = p.swapGas + p.gasOverhead + s.crossGas;
        uint256 floor = s.size + _toStart((gasStart - gasleft() + gasAhead) * tx.gasprice + p.l1Bound, p.rate) + p.minNet;
        if (s.out < floor) {
            emit NoTrade(msg.sender, BELOW_FLOOR, uint8(s.steps));
            return;
        }
        if (IERC20Min3(token).balanceOf(_lender(p.lender)) < s.size) {
            emit NoTrade(msg.sender, LENDER_SHORT, uint8(s.steps));
            return;
        }

        _tstore(T_SIZE, s.size);
        _tstore(T_GAS, gasStart - gasleft());
        _tstore(T_CALLER, uint256(uint160(msg.sender)) | (s.steps << 160) | (uint256(s.flags) << 168));
        _tstore(T_LOAN, 1);
        if (p.lender == 0) {
            address[] memory tokens = new address[](1);
            tokens[0] = token;
            uint256[] memory amounts = new uint256[](1);
            amounts[0] = s.size;
            IBalancerVault3(BALANCER_VAULT).flashLoan(address(this), tokens, amounts, plan);
        } else if (p.lender == 1) {
            IMorphoFlashLoan3(MORPHO_BLUE).flashLoan(token, s.size, plan);
        } else {
            IPoolManager3(POOL_MANAGER).unlock(plan);
        }
    }

    /// Balancer V2's flash loan callback. Only the loan `run` started is
    /// served: `run` sets the flag just before calling the lender, and the
    /// callback clears it first.
    function receiveFlashLoan(address[] calldata, uint256[] calldata, uint256[] calldata fees, bytes calldata plan)
        external
    {
        uint256 entry = gasleft();
        if (_tload(T_LOAN) == 0) revert UnexpectedCallback();
        _tstore(T_LOAN, 0);
        Plan3 memory p = LeanPlan3Codec.decode(plan);
        _transfer(_startToken(p.start), BALANCER_VAULT, _trade(p, fees[0], entry));
    }

    /// Morpho Blue's flash loan callback: no fee, and Morpho pulls its
    /// repayment after this returns (the constructor's approvals).
    function onMorphoFlashLoan(uint256, bytes calldata plan) external {
        uint256 entry = gasleft();
        if (_tload(T_LOAN) == 0) revert UnexpectedCallback();
        _tstore(T_LOAN, 0);
        _trade(LeanPlan3Codec.decode(plan), 0, entry);
    }

    /// PoolManager's unlock: the loan taken, the trade, the loan settled.
    /// Only PoolManager, and only the unlock `run` started.
    function unlockCallback(bytes calldata plan) external returns (bytes memory) {
        uint256 entry = gasleft();
        if (msg.sender != POOL_MANAGER || _tload(T_LOAN) == 0) revert UnexpectedCallback();
        _tstore(T_LOAN, 0);
        Plan3 memory p = LeanPlan3Codec.decode(plan);
        address token = _startToken(p.start);
        IPoolManager3(POOL_MANAGER).take(token, address(this), _tload(T_SIZE));
        _settle(token, _trade(p, 0, entry));
        return "";
    }

    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        _pay(amount0Delta, amount1Delta);
    }

    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        _pay(amount0Delta, amount1Delta);
    }

    /// Both hops at the size `run` chose, then the floor on what they returned
    /// and the profit, in the start token. `entry` is the gas left when the
    /// callback began. Returns what the lender is owed.
    function _trade(Plan3 memory p, uint256 lenderFee, uint256 entry) private returns (uint256 owed) {
        address token = _startToken(p.start);
        uint256 size = _tload(T_SIZE);
        uint256 out = _hop(p.hops[1], p.hops[0].tokenOut, _hop(p.hops[0], token, size));
        owed = size + lenderFee;
        uint256 costWei = (_tload(T_GAS) + entry - gasleft() + p.gasOverhead) * tx.gasprice + p.l1Bound;
        uint256 required = owed + _toStart(costWei, p.rate) + p.minNet;
        if (out < required) revert Floor(out, required);
        _transfer(token, profitRecipient, out - owed);
        uint256 caller = _tload(T_CALLER);
        emit Traded(address(uint160(caller)), p.start, size, out, lenderFee, costWei, uint8(caller >> 160), uint8(caller >> 168));
    }

    /// One exact-input swap of `amountIn` of `tokenIn`: what it returned.
    function _hop(Hop3 memory h, address tokenIn, uint256 amountIn) private returns (uint256 out) {
        if (h.kind == 3) {
            return _v4Hop(
                PoolKey3(_lower(tokenIn, h.tokenOut), _higher(tokenIn, h.tokenOut), h.fee, h.tickSpacing, h.pool),
                tokenIn,
                amountIn
            );
        }
        bool zeroForOne = tokenIn < h.tokenOut;
        if (h.kind == 2) {
            // Aerodrome v2: paid first, then asked for exactly what it quotes.
            out = IAeroSwap3(h.pool).getAmountOut(amountIn, tokenIn);
            _transfer(tokenIn, h.pool, amountIn);
            if (zeroForOne) {
                IAeroSwap3(h.pool).swap(0, out, address(this), "");
            } else {
                IAeroSwap3(h.pool).swap(out, 0, address(this), "");
            }
            return out;
        }
        _tstore(T_POOL, uint160(h.pool));
        _tstore(T_TOKEN_IN, uint160(tokenIn));
        (int256 a0, int256 a1) =
            IV3Pool3(h.pool).swap(address(this), zeroForOne, int256(amountIn), zeroForOne ? MIN_LIMIT : MAX_LIMIT, "");
        // The pool paid itself through its callback, or it was not paid.
        if (_tload(T_POOL) != 0) revert UnexpectedCallback();
        out = uint256(-(zeroForOne ? a1 : a0));
    }

    /// One exact-input v4 swap inside the trade's unlock: swapped, its input
    /// settled, its output taken. Returns what it paid out.
    function _v4Hop(PoolKey3 memory key, address tokenIn, uint256 amountIn) private returns (uint256 out) {
        bool zeroForOne = tokenIn == key.currency0;
        int256 delta = IPoolManager3(POOL_MANAGER).swap(
            key, SwapParams3(zeroForOne, -int256(amountIn), zeroForOne ? MIN_LIMIT : MAX_LIMIT), ""
        );
        int128 amount0 = int128(delta >> 128);
        int128 amount1 = int128(delta);
        (int128 paid, int128 got) = zeroForOne ? (amount0, amount1) : (amount1, amount0);
        if (paid > 0 || got <= 0) revert V4Delta();
        _settle(tokenIn, uint256(int256(-paid)));
        out = uint256(int256(got));
        IPoolManager3(POOL_MANAGER).take(zeroForOne ? key.currency1 : key.currency0, address(this), out);
    }

    /// Pay PoolManager `amount` of `token` against what is owed it.
    function _settle(address token, uint256 amount) private {
        IPoolManager3(POOL_MANAGER).sync(token);
        _transfer(token, POOL_MANAGER, amount);
        IPoolManager3(POOL_MANAGER).settle();
    }

    /// Pay the pool being swapped with, and only it, once.
    function _pay(int256 amount0Delta, int256 amount1Delta) private {
        if (msg.sender != address(uint160(_tload(T_POOL)))) revert UnexpectedCallback();
        _tstore(T_POOL, 0);
        _transfer(address(uint160(_tload(T_TOKEN_IN))), msg.sender, uint256(amount0Delta > 0 ? amount0Delta : amount1Delta));
    }

    /// A wei amount in the start token, rounded up: 1:1 for WETH (rate 0).
    function _toStart(uint256 wei_, uint256 rate) private pure returns (uint256) {
        return rate == 0 ? wei_ : LeanSwapMath3.mulDivUp(wei_, rate, 1e18);
    }

    function _lender(uint8 lender) private pure returns (address) {
        return lender == 0 ? BALANCER_VAULT : lender == 1 ? MORPHO_BLUE : POOL_MANAGER;
    }

    function _startToken(uint8 start) private pure returns (address) {
        return start == 0 ? WETH : USDC;
    }

    function _lower(address a, address b) private pure returns (address) {
        return a < b ? a : b;
    }

    function _higher(address a, address b) private pure returns (address) {
        return a < b ? b : a;
    }

    /// A transfer that tolerates tokens returning nothing, and refuses one
    /// returning false.
    function _transfer(address token, address to, uint256 amount) private {
        (bool ok, bytes memory data) = token.call(abi.encodeWithSelector(IERC20Min3.transfer.selector, to, amount));
        if (!ok || (data.length != 0 && !abi.decode(data, (bool)))) revert TransferFailed();
    }

    function _tstore(uint256 slot, uint256 value) private {
        assembly {
            tstore(slot, value)
        }
    }

    function _tload(uint256 slot) private view returns (uint256 value) {
        assembly {
            value := tload(slot)
        }
    }
}
