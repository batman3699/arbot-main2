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

    function setShort(bool s) external {
        short = s;
    }

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
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

    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160, bytes calldata)
        external
        returns (int256 amount0, int256 amount1)
    {
        require(zeroForOne == (tokenIn == token0), "direction");
        require(amountSpecified > 0, "exact input only");
        lastAmountSpecified = amountSpecified;
        uint256 amountIn = uint256(amountSpecified);
        uint256 out = amountIn * num / den;
        LeanToken(tokenOut).transfer(recipient, out);
        (amount0, amount1) = zeroForOne ? (amountSpecified, -int256(out)) : (-int256(out), amountSpecified);
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
        LeanToken(tokenOut).transfer(to, out);
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
