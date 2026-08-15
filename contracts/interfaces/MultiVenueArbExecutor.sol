// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

interface IUniswapV3Pool {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function fee() external view returns (uint24);
    function tickSpacing() external view returns (int24);
    function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16, uint16, uint16, uint8, bool);
    function flash(address recipient, uint256 amount0, uint256 amount1, bytes calldata data) external;
    function mint(address recipient, int24 tickLower, int24 tickUpper, uint128 amount, bytes calldata data)
        external
        returns (uint256 amount0, uint256 amount1);
    function burn(int24 tickLower, int24 tickUpper, uint128 amount) external returns (uint256 amount0, uint256 amount1);
    function collect(
        address recipient,
        int24 tickLower,
        int24 tickUpper,
        uint128 amount0Requested,
        uint128 amount1Requested
    ) external returns (uint256 amount0, uint256 amount1);
}

interface IUniswapV2Pair {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

interface IERC3156FlashBorrower {
    function onFlashLoan(address initiator, address token, uint256 amount, uint256 fee, bytes calldata data)
        external
        returns (bytes32);
}

interface IERC3156FlashLender {
    function flashLoan(IERC3156FlashBorrower receiver, address token, uint256 amount, bytes calldata data)
        external
        returns (bool);
}

interface IMultiVenueArbExecutor {
    enum Op {
        UNIV3,
        BALANCER,
        GENERIC,
        BRIDGE,
        JIT_LP_ADD,
        JIT_LP_REMOVE
    }

    enum LoanProvider {
        BALANCER,
        AAVE,
        ERC3156,
        UNIV2,
        UNIV3
    }

    struct Step {
        Op op;
        bytes data;
    }

    struct Loan {
        address token;
        uint256 amount;
        LoanProvider provider;
        address providerAddr;
    }

    struct PlanLegacy {
        address loanToken;
        uint256 amountIn;
        LoanProvider loanProvider;
        uint16 cycleSlippageBps;
        Step[] steps;
        uint256 minProfit;
    }

    struct PlanV2 {
        Loan[] loans;
        uint16 cycleSlippageBps;
        Step[] steps;
        uint256 minProfit;
    }

    function start(PlanLegacy calldata plan) external returns (uint256 grossProfit);
    function startV2(PlanV2 calldata plan) external returns (uint256 grossProfit);
    function owner() external view returns (address);
}
