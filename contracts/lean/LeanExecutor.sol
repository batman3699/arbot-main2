// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {BadPlan, LeanPlanCodec} from "./LeanPlanCodec.sol";

interface IERC20Min {
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
}

/// Uniswap v3, both Slipstream factories and PancakeSwap v3 share it.
interface IV3Pool {
    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data)
        external
        returns (int256 amount0, int256 amount1);
}

interface IAeroPool {
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

interface IMorphoFlashLoan {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

interface IBalancerVault {
    function flashLoan(address recipient, address[] memory tokens, uint256[] memory amounts, bytes memory userData)
        external;
}

/// The lean executor (Task 8.5 R29): a WETH flash loan, pools called directly
/// and paid in their own callbacks, the profit to the owner's recipient, and
/// only the checks the operator kept on chain (2026-10-17): trader-only
/// access, the plan's deadline, the profit floor and the owner's circuit
/// breaker. Hop minimums stay as well.
///
/// Not upgradeable: a new address is cheaper than a proxy on every call.
contract LeanExecutor {
    address public constant WETH = 0x4200000000000000000000000000000000000006;
    address public constant BALANCER_VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    address public constant MORPHO_BLUE = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;

    /// No price limit: the hop minimum is the bound (`MIN_SQRT_RATIO + 1`,
    /// `MAX_SQRT_RATIO - 1`, as the routers pass for none).
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

    /// The gross profit of a trade, paid to `profitRecipient`.
    event Profit(uint256 gross);

    error NotOwner();
    error ZeroAddress();
    error NotTrader();
    error CircuitOpen();
    error Expired(uint256 deadline, uint256 timestamp);
    /// A lender or swap callback this executor did not ask for.
    error UnexpectedCallback();
    error HopShort(uint256 hop, uint256 out, uint256 minOut);
    error Floor(uint256 out, uint256 required);
    error TransferFailed();

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    constructor(address owner_, address trader_, address profitRecipient_) {
        if (owner_ == address(0) || trader_ == address(0) || profitRecipient_ == address(0)) revert ZeroAddress();
        owner = owner_;
        executors[trader_] = true;
        profitRecipient = profitRecipient_;
        // Morpho Blue pulls its flash loan's repayment.
        IERC20Min(WETH).approve(MORPHO_BLUE, type(uint256).max);
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
    /// plan's shape, its deadline.
    function run(bytes calldata plan) external {
        if (!executors[msg.sender]) revert NotTrader();
        if (circuitOpen) revert CircuitOpen();
        (uint8 lender, uint256 amount, uint256 deadline,, uint256 hops) = LeanPlanCodec.header(plan);
        if (block.timestamp > deadline) revert Expired(deadline, block.timestamp);
        if (lender > 1) revert BadPlan();
        address last;
        for (uint256 i; i < hops; ++i) {
            (uint8 kind,, address tokenOut,) = LeanPlanCodec.hop(plan, i);
            if (kind > 2) revert BadPlan();
            last = tokenOut;
        }
        if (last != WETH) revert BadPlan();

        _tstore(T_LOAN, 1);
        if (lender == 0) {
            address[] memory tokens = new address[](1);
            tokens[0] = WETH;
            uint256[] memory amounts = new uint256[](1);
            amounts[0] = amount;
            IBalancerVault(BALANCER_VAULT).flashLoan(address(this), tokens, amounts, plan);
        } else {
            IMorphoFlashLoan(MORPHO_BLUE).flashLoan(WETH, amount, plan);
        }
    }

    /// Balancer V2's flash loan callback. Anyone can ask the vault for a loan
    /// naming this contract, so only the loan `run` started is served. The
    /// flag alone decides it: `run` sets it just before calling the lender, the
    /// callback clears it first, and between the two only the lender's WETH
    /// transfer runs, so no one else can call back while it is set.
    function receiveFlashLoan(address[] calldata, uint256[] calldata, uint256[] calldata fees, bytes calldata plan)
        external
    {
        if (_tload(T_LOAN) == 0) revert UnexpectedCallback();
        _tstore(T_LOAN, 0);
        _transfer(WETH, BALANCER_VAULT, _trade(plan, fees[0]));
    }

    /// Morpho Blue's flash loan callback: no fee, and Morpho pulls its
    /// repayment after this returns (the constructor's approval).
    function onMorphoFlashLoan(uint256, bytes calldata plan) external {
        if (_tload(T_LOAN) == 0) revert UnexpectedCallback();
        _tstore(T_LOAN, 0);
        _trade(plan, 0);
    }

    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        _pay(amount0Delta, amount1Delta);
    }

    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        _pay(amount0Delta, amount1Delta);
    }

    /// The hops, then the floor and the profit. Returns what the lender is owed.
    function _trade(bytes calldata plan, uint256 fee) private returns (uint256 owed) {
        (, uint256 amount,, uint256 minProfit, uint256 hops) = LeanPlanCodec.header(plan);
        uint256 out = amount;
        address tokenIn = WETH;
        for (uint256 i; i < hops; ++i) {
            (uint8 kind, address pool, address tokenOut, uint256 minOut) = LeanPlanCodec.hop(plan, i);
            out = _hop(kind, pool, tokenIn, tokenOut, out, minOut);
            if (out < minOut) revert HopShort(i, out, minOut);
            tokenIn = tokenOut;
        }
        owed = amount + fee;
        uint256 required = owed + minProfit;
        if (out < required) revert Floor(out, required);
        uint256 gross = out - owed;
        _transfer(WETH, profitRecipient, gross);
        emit Profit(gross);
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
                IAeroPool(pool).swap(0, minOut, address(this), "");
            } else {
                IAeroPool(pool).swap(minOut, 0, address(this), "");
            }
            return minOut;
        }
        _tstore(T_POOL, uint160(pool));
        _tstore(T_TOKEN_IN, uint160(tokenIn));
        (int256 a0, int256 a1) =
            IV3Pool(pool).swap(address(this), zeroForOne, int256(amountIn), zeroForOne ? MIN_LIMIT : MAX_LIMIT, "");
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

    /// A transfer that tolerates tokens returning nothing, and refuses one
    /// returning false.
    function _transfer(address token, address to, uint256 amount) private {
        (bool ok, bytes memory data) = token.call(abi.encodeWithSelector(IERC20Min.transfer.selector, to, amount));
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
