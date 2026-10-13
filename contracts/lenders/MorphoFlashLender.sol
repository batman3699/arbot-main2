// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

interface IMorphoFlash {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

interface IERC20Calls {
    function balanceOf(address account) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function approve(address spender, uint256 amount) external returns (bool);
    function allowance(address owner, address spender) external view returns (uint256);
}

interface IERC3156Borrower {
    function onFlashLoan(address initiator, address token, uint256 amount, uint256 fee, bytes calldata data)
        external
        returns (bytes32);
}

/// Morpho Blue's flash loan as an ERC-3156 lender (Task 8.5 R27).
///
/// The deployed executor settles an ERC-3156 loan correctly: it repays
/// `amount + fee` by allowance after its callback. It cannot settle Aave's,
/// because it counts Aave's later pull as profit. Morpho Blue lends any token it
/// holds, free, through its own callback; this contract translates.
///
/// Immutable, ownerless and stateless. It holds a loan for one transaction and
/// nothing between. Anyone may borrow through it, as anyone may from Morpho.
///
/// No state, because none is needed: Morpho Blue calls back only the caller of
/// its `flashLoan`, so a callback from Morpho to this contract is for a loan this
/// contract started, carrying the context it encoded. That saves the
/// executor's settlement two storage writes and their clears on every loan:
/// gas is the price-per-gas divisor in a priority auction (R27).
contract MorphoFlashLender {
    /// Morpho Blue on Base: 15,623 bytes of code, `flashLoan` selector
    /// `0xe0232b42` (read 2026-10-13). A constant, so the compiled runtime code
    /// is the deployed code.
    address public constant MORPHO = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    bytes32 public constant CALLBACK_SUCCESS = keccak256("ERC3156FlashBorrower.onFlashLoan");

    error NotMorpho();
    error CallbackFailed();
    error TokenCallFailed();

    function maxFlashLoan(address token) external view returns (uint256) {
        return IERC20Calls(token).balanceOf(MORPHO);
    }

    function flashFee(address, uint256) external pure returns (uint256) {
        return 0;
    }

    function flashLoan(address borrower, address token, uint256 amount, bytes calldata data) external returns (bool) {
        IMorphoFlash(MORPHO).flashLoan(token, amount, abi.encode(borrower, msg.sender, token, data));
        return true;
    }

    function onMorphoFlashLoan(uint256 assets, bytes calldata context) external {
        if (msg.sender != MORPHO) revert NotMorpho();
        (address borrower, address initiator, address token, bytes memory data) =
            abi.decode(context, (address, address, address, bytes));
        _call(token, abi.encodeCall(IERC20Calls.transfer, (borrower, assets)));
        if (IERC3156Borrower(borrower).onFlashLoan(initiator, token, assets, 0, data) != CALLBACK_SUCCESS) {
            revert CallbackFailed();
        }
        _call(token, abi.encodeCall(IERC20Calls.transferFrom, (borrower, address(this), assets)));
        // Morpho pulls the assets back once this returns. Approved once, for
        // good: this contract holds nothing between loans, and a token that
        // keeps a maximal allowance (WETH9) then writes no allowance per loan.
        if (IERC20Calls(token).allowance(address(this), MORPHO) < assets) {
            _call(token, abi.encodeCall(IERC20Calls.approve, (MORPHO, type(uint256).max)));
        }
    }

    /// A token call that must succeed and, when it returns data, return true.
    function _call(address token, bytes memory data) private {
        (bool ok, bytes memory ret) = token.call(data);
        if (!ok || (ret.length != 0 && (ret.length < 32 || !abi.decode(ret, (bool))))) {
            revert TokenCallFailed();
        }
    }
}
