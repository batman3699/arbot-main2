// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {BadPlan} from "./LeanPlanCodec.sol";
import {LeanPlan2Codec} from "./LeanPlan2Codec.sol";

interface IERC20Min2 {
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
}

/// Uniswap v3, both Slipstream factories and PancakeSwap v3 share it.
interface IV3Pool2 {
    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data)
        external
        returns (int256 amount0, int256 amount1);
}

interface IAeroPool2 {
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

interface IMorphoFlashLoan2 {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

interface IBalancerVault2 {
    function flashLoan(address recipient, address[] memory tokens, uint256[] memory amounts, bytes memory userData)
        external;
}

/// Uniswap v4's PoolKey and SwapParams, as PoolManager takes them.
struct PoolKey {
    address currency0;
    address currency1;
    uint24 fee;
    int24 tickSpacing;
    address hooks;
}

struct SwapParams {
    bool zeroForOne;
    int256 amountSpecified;
    uint160 sqrtPriceLimitX96;
}

/// The five PoolManager calls a trade makes, and nothing else.
interface IPoolManager2 {
    function unlock(bytes calldata data) external returns (bytes memory);
    function swap(PoolKey memory key, SwapParams memory params, bytes calldata hookData) external returns (int256);
    function sync(address currency) external;
    function settle() external payable returns (uint256);
    function take(address currency, address to, uint256 amount) external;
}

/// The second lean executor (Task 8.5 R37): v1 (R29) with a start token, WETH
/// or USDC, Uniswap v4 hops, and PoolManager's flash accounting as a third
/// lender. A trade under PoolManager runs inside one unlock: the start amount
/// is taken from it, each v4 hop is swapped and settled at once, and the loan
/// is settled at the end. A v4 hop runs only under that lender.
///
/// The operator's on-chain checks are v1's: trader-only access, the plan's
/// deadline, the hop minimums, the profit floor and the owner's circuit
/// breaker. Not upgradeable.
contract LeanExecutor2 {
    address public constant WETH = 0x4200000000000000000000000000000000000006;
    address public constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address public constant BALANCER_VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    address public constant MORPHO_BLUE = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    address public constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;

    /// No price limit: the hop minimum is the bound (`MIN_SQRT_RATIO + 1`,
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

    address public owner;
    mapping(address => bool) public executors;
    /// Read with the circuit on every trade: one slot, one cold read.
    address public profitRecipient;
    bool private circuitOpen;

    /// The gross profit of a trade, in its start token, paid to
    /// `profitRecipient`.
    event Profit(uint256 gross);

    error NotOwner();
    error ZeroAddress();
    error NotTrader();
    error CircuitOpen();
    error Expired(uint256 deadline, uint256 timestamp);
    /// A lender, unlock or swap callback this executor did not ask for.
    error UnexpectedCallback();
    error HopShort(uint256 hop, uint256 out, uint256 minOut);
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
        IERC20Min2(WETH).approve(MORPHO_BLUE, type(uint256).max);
        IERC20Min2(USDC).approve(MORPHO_BLUE, type(uint256).max);
    }

    /// The owner's stop, read by the runtime every block and by every trade.
    function isCircuitOpen() external view returns (bool) {
        return circuitOpen;
    }

    function setExecutor(address account, bool allowed) external onlyOwner {
        if (account == address(0)) revert ZeroAddress();
        executors[account] = allowed;
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

    /// One trade. The checks the operator kept, cheapest first and all before
    /// the loan, so a stale plan costs little: the trader, the circuit, the
    /// deadline, the plan's shape.
    function run(bytes calldata plan) external {
        if (!executors[msg.sender]) revert NotTrader();
        if (circuitOpen) revert CircuitOpen();
        (uint8 lender, uint8 start, uint256 amount, uint256 deadline,) = LeanPlan2Codec.header(plan);
        if (block.timestamp > deadline) revert Expired(deadline, block.timestamp);
        if (lender > 2 || start > 1) revert BadPlan();
        address last;
        uint256 offset = LeanPlan2Codec.HEADER;
        while (offset < plan.length) {
            (uint8 kind,, address tokenOut,,,, uint256 next) = LeanPlan2Codec.hopAt(plan, offset);
            if (kind > 3 || (kind == 3 && lender != 2)) revert BadPlan();
            last = tokenOut;
            offset = next;
        }
        address token = _startToken(start);
        if (last != token) revert BadPlan();

        _tstore(T_LOAN, 1);
        if (lender == 0) {
            address[] memory tokens = new address[](1);
            tokens[0] = token;
            uint256[] memory amounts = new uint256[](1);
            amounts[0] = amount;
            IBalancerVault2(BALANCER_VAULT).flashLoan(address(this), tokens, amounts, plan);
        } else if (lender == 1) {
            IMorphoFlashLoan2(MORPHO_BLUE).flashLoan(token, amount, plan);
        } else {
            IPoolManager2(POOL_MANAGER).unlock(plan);
        }
    }

    /// Balancer V2's flash loan callback. Only the loan `run` started is
    /// served: `run` sets the flag just before calling the lender, and the
    /// callback clears it first.
    function receiveFlashLoan(address[] calldata, uint256[] calldata, uint256[] calldata fees, bytes calldata plan)
        external
    {
        if (_tload(T_LOAN) == 0) revert UnexpectedCallback();
        _tstore(T_LOAN, 0);
        (, uint8 start,,,) = LeanPlan2Codec.header(plan);
        _transfer(_startToken(start), BALANCER_VAULT, _trade(plan, fees[0]));
    }

    /// Morpho Blue's flash loan callback: no fee, and Morpho pulls its
    /// repayment after this returns (the constructor's approvals).
    function onMorphoFlashLoan(uint256, bytes calldata plan) external {
        if (_tload(T_LOAN) == 0) revert UnexpectedCallback();
        _tstore(T_LOAN, 0);
        _trade(plan, 0);
    }

    /// PoolManager's unlock: the loan taken, the trade, the loan settled.
    /// Only PoolManager, and only the unlock `run` started.
    function unlockCallback(bytes calldata plan) external returns (bytes memory) {
        if (msg.sender != POOL_MANAGER || _tload(T_LOAN) == 0) revert UnexpectedCallback();
        _tstore(T_LOAN, 0);
        (, uint8 start, uint256 amount,,) = LeanPlan2Codec.header(plan);
        address token = _startToken(start);
        IPoolManager2(POOL_MANAGER).take(token, address(this), amount);
        uint256 owed = _trade(plan, 0);
        _settle(token, owed);
        return "";
    }

    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        _pay(amount0Delta, amount1Delta);
    }

    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        _pay(amount0Delta, amount1Delta);
    }

    /// The hops, then the floor and the profit, in the start token. Returns
    /// what the lender is owed.
    function _trade(bytes calldata plan, uint256 lenderFee) private returns (uint256 owed) {
        (, uint8 start, uint256 amount,, uint256 minProfit) = LeanPlan2Codec.header(plan);
        address token = _startToken(start);
        uint256 out = amount;
        address tokenIn = token;
        uint256 offset = LeanPlan2Codec.HEADER;
        for (uint256 i; offset < plan.length; ++i) {
            (uint8 kind, address pool, address tokenOut, uint256 minOut, uint24 fee, int24 spacing, uint256 next) =
                LeanPlan2Codec.hopAt(plan, offset);
            out = kind == 3
                ? _v4Hop(PoolKey(_lower(tokenIn, tokenOut), _higher(tokenIn, tokenOut), fee, spacing, pool), tokenIn, out)
                : _hop(kind, pool, tokenIn, tokenOut, out, minOut);
            if (out < minOut) revert HopShort(i, out, minOut);
            tokenIn = tokenOut;
            offset = next;
        }
        owed = amount + lenderFee;
        uint256 required = owed + minProfit;
        if (out < required) revert Floor(out, required);
        uint256 gross = out - owed;
        _transfer(token, profitRecipient, gross);
        emit Profit(gross);
    }

    /// One exact-input v4 swap of `amountIn` of `tokenIn`, inside the trade's
    /// unlock: swapped, its input settled, its output taken. Returns what it
    /// paid out.
    function _v4Hop(PoolKey memory key, address tokenIn, uint256 amountIn) private returns (uint256 out) {
        bool zeroForOne = tokenIn == key.currency0;
        int256 delta = IPoolManager2(POOL_MANAGER).swap(
            key, SwapParams(zeroForOne, -int256(amountIn), zeroForOne ? MIN_LIMIT : MAX_LIMIT), ""
        );
        int128 amount0 = int128(delta >> 128);
        int128 amount1 = int128(delta);
        (int128 paid, int128 got) = zeroForOne ? (amount0, amount1) : (amount1, amount0);
        if (paid > 0 || got <= 0) revert V4Delta();
        _settle(tokenIn, uint256(int256(-paid)));
        out = uint256(int256(got));
        IPoolManager2(POOL_MANAGER).take(key.currency0 == tokenIn ? key.currency1 : key.currency0, address(this), out);
    }

    /// Pay PoolManager `amount` of `token` against what is owed it.
    function _settle(address token, uint256 amount) private {
        IPoolManager2(POOL_MANAGER).sync(token);
        _transfer(token, POOL_MANAGER, amount);
        IPoolManager2(POOL_MANAGER).settle();
    }

    /// One swap of `amountIn` of `tokenIn` for `tokenOut` on `pool`: what it
    /// returned. Aerodrome v2 is asked for exactly `minOut`.
    function _hop(uint8 kind, address pool, address tokenIn, address tokenOut, uint256 amountIn, uint256 minOut)
        internal
        returns (uint256 out)
    {
        bool zeroForOne = tokenIn < tokenOut;
        if (kind == 2) {
            // Aerodrome v2: paid first, then asked for exactly the quote.
            _transfer(tokenIn, pool, amountIn);
            if (zeroForOne) {
                IAeroPool2(pool).swap(0, minOut, address(this), "");
            } else {
                IAeroPool2(pool).swap(minOut, 0, address(this), "");
            }
            return minOut;
        }
        _tstore(T_POOL, uint160(pool));
        _tstore(T_TOKEN_IN, uint160(tokenIn));
        (int256 a0, int256 a1) =
            IV3Pool2(pool).swap(address(this), zeroForOne, int256(amountIn), zeroForOne ? MIN_LIMIT : MAX_LIMIT, "");
        // The pool paid itself through its callback, or it was not paid.
        if (_tload(T_POOL) != 0) revert UnexpectedCallback();
        out = uint256(-(zeroForOne ? a1 : a0));
    }

    /// Pay the pool being swapped with, and only it, once.
    function _pay(int256 amount0Delta, int256 amount1Delta) private {
        if (msg.sender != address(uint160(_tload(T_POOL)))) revert UnexpectedCallback();
        _tstore(T_POOL, 0);
        _transfer(address(uint160(_tload(T_TOKEN_IN))), msg.sender, uint256(amount0Delta > 0 ? amount0Delta : amount1Delta));
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
        (bool ok, bytes memory data) = token.call(abi.encodeWithSelector(IERC20Min2.transfer.selector, to, amount));
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
