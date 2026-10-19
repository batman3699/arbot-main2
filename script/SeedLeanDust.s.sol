// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Script, console2} from "forge-std/Script.sol";

interface IWETH9S {
    function deposit() external payable;
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address who) external view returns (uint256);
}

interface IPoolS {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function fee() external view returns (uint24);
    function tickSpacing() external view returns (int24);
}

/// Uniswap's SwapRouter02 and PancakeSwap's SmartRouter.
interface IV3RouterS {
    struct Params {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(Params calldata) external payable returns (uint256);
}

interface ISlipstreamRouterS {
    struct Params {
        address tokenIn;
        address tokenOut;
        int24 tickSpacing;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(Params calldata) external payable returns (uint256);
}

interface IAeroRouterS {
    struct Route {
        address from;
        address to;
        bool stable;
        address factory;
    }

    function swapExactTokensForTokens(uint256, uint256, Route[] calldata, address, uint256)
        external
        returns (uint256[] memory);
}

/// R29: leave a few units of every middle token the canary trades through at
/// the lean executor, 1 wei of WETH there, and 1 wei of WETH at the owner, so
/// no trade pays to write a balance from zero (~17k gas a slot; the fork
/// measured 361,111 gas dusted against 378,211 cold on ticket 664's route).
///
/// Each token is bought with 10^12 wei of WETH (~$0.003) through its pool's
/// venue router: kind 0 Uniswap v3, 1 PancakeSwap v3, 2 Slipstream, 3
/// Slipstream V3, 4 Aerodrome v2. The pool is checked to pair the token with
/// WETH, and its fee or tick spacing is read from it.
///
///   forge script script/SeedLeanDust.s.sol --sig "run(address,address[],uint8[],address[])" \
///     <executor> <tokens> <kinds> <pools> --rpc-url https://mainnet.base.org --sender <owner>   # dry run
///   (the same, with --broadcast --interactive)                                                  # the owner
contract SeedLeanDust is Script {
    IWETH9S constant WETH = IWETH9S(0x4200000000000000000000000000000000000006);
    uint256 constant AMOUNT = 1e12;
    address constant UNISWAP_ROUTER = 0x2626664c2603336E57B271c5C0b26F421741e481;
    address constant PANCAKE_ROUTER = 0x678Aa4bF4E210cf2166753e054d5b7c31cc7fa86;
    address constant SLIPSTREAM_ROUTER = 0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5;
    address constant SLIPSTREAM_V3_ROUTER = 0x698Cb2b6dd822994581fEa6eA4Fc755d1363A92F;
    address constant AERODROME_ROUTER = 0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43;
    address constant AERODROME_FACTORY = 0x420DD381b31aEf6683db6B902084cB0FFECe40Da;

    function run(address executor, address[] calldata tokens, uint8[] calldata kinds, address[] calldata pools)
        external
    {
        require(block.chainid == 8453, "Base only");
        vm.startBroadcast();
        _seed(executor, tokens, kinds, pools);
        vm.stopBroadcast();
        _check(executor, tokens);
    }

    /// The same, broadcast as `who`: what a test runs.
    function seedAs(address who, address executor, address[] calldata tokens, uint8[] calldata kinds, address[] calldata pools)
        external
    {
        vm.startBroadcast(who);
        _seed(executor, tokens, kinds, pools);
        vm.stopBroadcast();
        _check(executor, tokens);
    }

    function _seed(address executor, address[] calldata tokens, uint8[] calldata kinds, address[] calldata pools) private {
        require(tokens.length == kinds.length && tokens.length == pools.length, "one kind and pool per token");
        WETH.deposit{value: AMOUNT * tokens.length + 2}();
        for (uint256 i; i < tokens.length; ++i) {
            _buy(executor, tokens[i], kinds[i], pools[i]);
        }
        require(WETH.transfer(executor, 1), "WETH dust");
    }

    function _buy(address executor, address token, uint8 kind, address pool) private {
        IPoolS p = IPoolS(pool);
        require(
            (p.token0() == address(WETH) && p.token1() == token) || (p.token1() == address(WETH) && p.token0() == token),
            "the pool does not pair the token with WETH"
        );
        if (kind == 0 || kind == 1) {
            address router = kind == 0 ? UNISWAP_ROUTER : PANCAKE_ROUTER;
            WETH.approve(router, AMOUNT);
            IV3RouterS(router).exactInputSingle(IV3RouterS.Params(address(WETH), token, p.fee(), executor, AMOUNT, 0, 0));
        } else if (kind == 2 || kind == 3) {
            address router = kind == 2 ? SLIPSTREAM_ROUTER : SLIPSTREAM_V3_ROUTER;
            WETH.approve(router, AMOUNT);
            ISlipstreamRouterS(router).exactInputSingle(
                ISlipstreamRouterS.Params(address(WETH), token, p.tickSpacing(), executor, block.timestamp + 3600, AMOUNT, 0, 0)
            );
        } else {
            require(kind == 4, "an unknown kind");
            WETH.approve(AERODROME_ROUTER, AMOUNT);
            IAeroRouterS.Route[] memory route = new IAeroRouterS.Route[](1);
            route[0] = IAeroRouterS.Route(address(WETH), token, false, AERODROME_FACTORY);
            IAeroRouterS(AERODROME_ROUTER).swapExactTokensForTokens(AMOUNT, 0, route, executor, block.timestamp + 3600);
        }
    }

    function _check(address executor, address[] calldata tokens) private view {
        for (uint256 i; i < tokens.length; ++i) {
            uint256 held = IWETH9S(tokens[i]).balanceOf(executor);
            require(held > 0, "a token was not seeded");
            console2.log(tokens[i], held);
        }
        require(WETH.balanceOf(executor) > 0, "no WETH at the executor");
        console2.log("executor WETH (wei)", WETH.balanceOf(executor));
    }
}
