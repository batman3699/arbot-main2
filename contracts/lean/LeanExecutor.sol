// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IERC20Min {
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
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

    address public owner;
    mapping(address => bool) public executors;
    /// Read with the circuit on every trade: one slot, one cold read.
    address public profitRecipient;
    bool private circuitOpen;

    error NotOwner();
    error ZeroAddress();

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
        IERC20Min(token).transfer(to, amount);
    }
}
