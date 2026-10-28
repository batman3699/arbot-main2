// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// Test doubles for the lean executor (Task 8.5 R29). Each holds the venue's
/// rule that matters to the executor: a v3 pool pays first and checks it was
/// paid in full after its callback; an Aerodrome v2 pool pays what it is asked
/// if what it received covers it; Balancer's vault lends, calls back, and
/// checks it was repaid.

contract LeanToken {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    /// Delivers one unit less than every transfer asks (a fee-on-transfer token).
    bool public short;
    /// `transfer` returns false and moves nothing.
    bool public refuses;

    function setShort(bool s) external {
        short = s;
    }

    function setRefuses(bool r) external {
        refuses = r;
    }

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        if (refuses) return false;
        _move(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 allowed = allowance[from][msg.sender];
        if (allowed != type(uint256).max) {
            require(allowed >= amount, "allowance");
            allowance[from][msg.sender] = allowed - amount;
        }
        _move(from, to, amount);
        return true;
    }

    function _move(address from, address to, uint256 amount) private {
        require(balanceOf[from] >= amount, "balance");
        balanceOf[from] -= amount;
        balanceOf[to] += short && amount > 0 ? amount - 1 : amount;
    }
}

/// A token whose `transfer` and `transferFrom` return nothing, as older ones do.
contract SilentToken {
    mapping(address => uint256) public balanceOf;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function transfer(address to, uint256 amount) external {
        require(balanceOf[msg.sender] >= amount, "balance");
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
    }
}

/// Pay out without trusting a return value: the token may return none.
function pay(address token, address to, uint256 amount) {
    (bool ok,) = token.call(abi.encodeWithSelector(LeanToken.transfer.selector, to, amount));
    require(ok, "payout");
}

interface IVaultLoan {
    function flashLoan(address recipient, address[] memory tokens, uint256[] memory amounts, bytes memory userData) external;
}

interface IV3Callback {
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external;
    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external;
}

/// Calls an executor's swap callback as if it were a pool.
contract Poker {
    function poke(address executor, int256 a0, int256 a1) external {
        IV3Callback(executor).uniswapV3SwapCallback(a0, a1, "");
    }
}

/// A v3 pool trading one way, `tokenIn` for `tokenOut` at `num / den`.
contract MockV3Pool {
    address public immutable token0;
    address public immutable token1;
    address public immutable tokenIn;
    address public immutable tokenOut;
    uint256 public immutable num;
    uint256 public immutable den;
    bool public immutable pancake;

    int256 public lastAmountSpecified;
    bool public callTwice;
    /// Pays out and never calls back.
    bool public skipCallback;
    /// Before calling back, asks the vault for a loan naming its caller.
    bytes public nestedPlan;
    /// When set, the callback comes from this contract instead of the pool.
    Poker public delegate;

    constructor(address tokenIn_, address tokenOut_, uint256 num_, uint256 den_, bool pancake_) {
        (tokenIn, tokenOut, num, den, pancake) = (tokenIn_, tokenOut_, num_, den_, pancake_);
        (token0, token1) = tokenIn_ < tokenOut_ ? (tokenIn_, tokenOut_) : (tokenOut_, tokenIn_);
    }

    function setCallTwice(bool t) external {
        callTwice = t;
    }

    function setDelegate(Poker p) external {
        delegate = p;
    }

    function setSkipCallback(bool s) external {
        skipCallback = s;
    }

    function setNestedPlan(bytes calldata p) external {
        nestedPlan = p;
    }

    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160, bytes calldata)
        external
        returns (int256 amount0, int256 amount1)
    {
        require(zeroForOne == (tokenIn == token0), "direction");
        require(amountSpecified > 0, "exact input only");
        lastAmountSpecified = amountSpecified;
        uint256 amountIn = uint256(amountSpecified);
        uint256 out = amountIn * num / den;
        pay(tokenOut, recipient, out);
        (amount0, amount1) = zeroForOne ? (amountSpecified, -int256(out)) : (-int256(out), amountSpecified);
        if (skipCallback) return (amount0, amount1);
        uint256 before = LeanToken(tokenIn).balanceOf(address(this));
        if (address(delegate) != address(0)) {
            delegate.poke(msg.sender, amount0, amount1);
        } else if (pancake) {
            IV3Callback(msg.sender).pancakeV3SwapCallback(amount0, amount1, "");
        } else {
            IV3Callback(msg.sender).uniswapV3SwapCallback(amount0, amount1, "");
        }
        if (callTwice) {
            IV3Callback(msg.sender).uniswapV3SwapCallback(amount0, amount1, "");
        }
        require(LeanToken(tokenIn).balanceOf(address(this)) >= before + amountIn, "IIA");
        // Paid, and before returning: the window the executor's loan flag must
        // already be closed in.
        if (nestedPlan.length != 0) {
            // Once: the nested trade swaps on this pool too.
            bytes memory nested = nestedPlan;
            delete nestedPlan;
            address[] memory tokens = new address[](1);
            tokens[0] = 0x4200000000000000000000000000000000000006;
            uint256[] memory amounts = new uint256[](1);
            amounts[0] = 1;
            IVaultLoan(0xBA12222222228d8Ba445958a75a0704d566BF2C8).flashLoan(msg.sender, tokens, amounts, nested);
        }
    }
}

/// An Aerodrome v2 pool trading `tokenIn` for `tokenOut` at `num / den`.
contract MockAeroPool {
    address public immutable token0;
    address public immutable tokenIn;
    address public immutable tokenOut;
    uint256 public immutable num;
    uint256 public immutable den;
    uint256 public seenIn;

    constructor(address tokenIn_, address tokenOut_, uint256 num_, uint256 den_) {
        (tokenIn, tokenOut, num, den) = (tokenIn_, tokenOut_, num_, den_);
        token0 = tokenIn_ < tokenOut_ ? tokenIn_ : tokenOut_;
    }

    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external {
        require(data.length == 0, "no hook");
        uint256 out = tokenOut == token0 ? amount0Out : amount1Out;
        require((tokenOut == token0 ? amount1Out : amount0Out) == 0, "one side out");
        uint256 received = LeanToken(tokenIn).balanceOf(address(this)) - seenIn;
        require(received * num / den >= out, "K");
        seenIn += received;
        pay(tokenOut, to, out);
    }
}

interface IFlashRecipient {
    function receiveFlashLoan(
        address[] calldata tokens,
        uint256[] calldata amounts,
        uint256[] calldata fees,
        bytes calldata userData
    ) external;
}

/// Balancer V2's vault, as far as a flash loan goes. Etched at the vault's
/// address, so its state is whatever the test sets.
contract MockVault {
    uint256 public calls;
    uint256 public fee;

    function setFee(uint256 f) external {
        fee = f;
    }

    function flashLoan(address recipient, address[] memory tokens, uint256[] memory amounts, bytes memory userData)
        external
    {
        calls++;
        uint256[] memory fees = new uint256[](1);
        fees[0] = fee;
        uint256 before = LeanToken(tokens[0]).balanceOf(address(this));
        LeanToken(tokens[0]).transfer(recipient, amounts[0]);
        IFlashRecipient(recipient).receiveFlashLoan(tokens, amounts, fees, userData);
        require(LeanToken(tokens[0]).balanceOf(address(this)) >= before + fee, "not repaid");
    }
}

/// Uniswap v4's PoolKey and SwapParams, as PoolManager takes them.
struct MockPoolKey {
    address currency0;
    address currency1;
    uint24 fee;
    int24 tickSpacing;
    address hooks;
}

struct MockSwapParams {
    bool zeroForOne;
    int256 amountSpecified;
    uint160 sqrtPriceLimitX96;
}

interface IUnlockCallbackMock {
    function unlockCallback(bytes calldata data) external returns (bytes memory);
}

/// Task 8.5 R37: Uniswap v4's PoolManager, as its flash accounting behaves.
/// Deltas are the caller's: `take` owes, `settle` pays what arrived since
/// `sync`, `swap` owes its input and credits its output; `unlock` reverts
/// unless every delta is back to zero. Storage only, so `vm.etch` can put it
/// at PoolManager's address. Exact input only, at a fixed rate a direction.
contract MockPoolManager {
    error CurrencyNotSettled();
    error ManagerLocked();

    bool public unlocked;
    mapping(address => int256) public delta;
    uint256 public nonzeroDeltas;
    address public synced;
    uint256 public syncedReserves;
    /// `tokenIn => tokenOut => (num, den)`.
    mapping(address => mapping(address => uint256[2])) public rate;
    MockPoolKey public lastKey;
    uint256 public swaps;
    /// A swap that reports its input as owed to the caller: a delta of the
    /// wrong sign, which the executor must refuse.
    bool public flipDelta;
    /// A swap that reports its output as owed by the caller.
    bool public negativeOut;

    function setNegativeOut(bool n) external {
        negativeOut = n;
    }

    function setRate(address tokenIn, address tokenOut, uint256 num, uint256 den) external {
        rate[tokenIn][tokenOut] = [num, den];
    }

    function setFlipDelta(bool f) external {
        flipDelta = f;
    }

    function unlock(bytes calldata data) external returns (bytes memory result) {
        if (unlocked) revert ManagerLocked();
        unlocked = true;
        result = IUnlockCallbackMock(msg.sender).unlockCallback(data);
        if (nonzeroDeltas != 0) revert CurrencyNotSettled();
        unlocked = false;
    }

    function swap(MockPoolKey memory key, MockSwapParams memory params, bytes calldata) external returns (int256) {
        if (!unlocked) revert ManagerLocked();
        require(key.currency0 < key.currency1, "unsorted key");
        require(params.amountSpecified < 0, "exact input only");
        (address tokenIn, address tokenOut) =
            params.zeroForOne ? (key.currency0, key.currency1) : (key.currency1, key.currency0);
        uint256[2] memory r = rate[tokenIn][tokenOut];
        require(r[1] != 0, "no such pool");
        lastKey = key;
        swaps++;
        uint256 amountIn = uint256(-params.amountSpecified);
        uint256 out = amountIn * r[0] / r[1];
        int128 owedIn = flipDelta ? int128(int256(amountIn)) : -int128(int256(amountIn));
        _account(tokenIn, owedIn);
        int128 credited = negativeOut ? -int128(int256(out)) : int128(int256(out));
        _account(tokenOut, credited);
        (int128 a0, int128 a1) = params.zeroForOne ? (owedIn, credited) : (credited, owedIn);
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
