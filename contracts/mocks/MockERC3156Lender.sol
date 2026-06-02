// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {IERC3156FlashLender, IERC3156FlashBorrower} from "../executor/MultiVenueArbImplementation.sol";
import {MockERC20} from "./MockERC20.sol";

contract ERC3156CallbackProxy {
    function proxyFlashLoanCallback(
        IERC3156FlashBorrower borrower,
        address initiator,
        address token,
        uint256 amount,
        uint256 fee,
        bytes calldata data
    ) external returns (bytes32) {
        return borrower.onFlashLoan(initiator, token, amount, fee, data);
    }
}

contract MockERC3156Lender is IERC3156FlashLender {
    MockERC20 public immutable token;
    uint256 public feeBps;

    ERC3156CallbackProxy public forwarder;
    address public overrideToken;
    bool public useForwarder;
    bool public useAltToken;

    constructor(address token_, uint256 feeBps_) {
        token = MockERC20(token_);
        feeBps = feeBps_;
    }

    function setForwarder(address forwarder_, bool enabled) external {
        forwarder = ERC3156CallbackProxy(forwarder_);
        useForwarder = enabled;
    }

    function setOverrideToken(address token_, bool enabled) external {
        overrideToken = token_;
        useAltToken = enabled;
    }

    function flashLoan(IERC3156FlashBorrower receiver, address token_, uint256 amount, bytes calldata data)
        external
        override
        returns (bool)
    {
        address loanToken = useAltToken ? overrideToken : token_;
        require(loanToken != address(0), "token");
        require(token_ == address(token), "unsupported");

        uint256 fee = (amount * feeBps) / 10_000;
        token.transfer(address(receiver), amount);

        bytes32 result;
        address initiator = msg.sender;
        if (useForwarder) {
            require(address(forwarder) != address(0), "forwarder");
            result = forwarder.proxyFlashLoanCallback(receiver, initiator, loanToken, amount, fee, data);
        } else {
            result = receiver.onFlashLoan(initiator, loanToken, amount, fee, data);
        }

        if (result != keccak256("ERC3156FlashBorrower.onFlashLoan")) revert("callback");
        require(token.allowance(address(receiver), address(this)) >= amount + fee, "approve");
        require(token.transferFrom(address(receiver), address(this), amount + fee), "repay");
        return true;
    }
}
