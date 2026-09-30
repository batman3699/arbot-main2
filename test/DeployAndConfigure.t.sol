// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Test} from "forge-std/Test.sol";
import {Deploy} from "../script/Deploy.s.sol";
import {DeployAndConfigure} from "../script/DeployAndConfigure.s.sol";
import {BatchRouter} from "../contracts/executor/BatchRouter.sol";
import {MultiVenueArbImplementation} from "../contracts/executor/MultiVenueArbImplementation.sol";

/// Stands in for Slipstream's router: `registerAdapter` refuses an address with
/// no code, and this test is about the wiring rather than the router.
contract SlipstreamRouterStub {
    function factory() external pure returns (address) {
        return address(0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A);
    }
}

/// `script/DeployAndConfigure.s.sol`: the Phase 5 executor deployed and wired for
/// the trading signer in one broadcast — and refused whenever the trading key
/// would deploy or own it (§18.1).
contract DeployAndConfigureTest is Test {
    DeployAndConfigure internal script;
    address internal admin;
    uint256 internal adminKey;
    address internal trader;
    uint256 internal traderKey;
    address internal slipstream;

    function setUp() external {
        script = new DeployAndConfigure();
        (admin, adminKey) = makeAddrAndKey("admin");
        (trader, traderKey) = makeAddrAndKey("trader");
        slipstream = address(new SlipstreamRouterStub());
    }

    function _cfg(uint256 key, address configuredOwner) internal pure returns (Deploy.DeployConfig memory) {
        return Deploy.DeployConfig({
            prefix: "",
            vault: address(0x1111111111111111111111111111111111111111),
            uniRouter: address(0x2222222222222222222222222222222222222222),
            aavePool: address(0x3333333333333333333333333333333333333333),
            permit2: address(0x4444444444444444444444444444444444444444),
            configuredOwner: configuredOwner,
            privateKey: key,
            salt: bytes32(uint256(0xC0FFEE)),
            requireBalancer: false,
            requireAave: false
        });
    }

    function _wiring() internal view returns (Deploy.Wiring memory) {
        return Deploy.Wiring({trader: trader, slipstreamRouter: slipstream});
    }

    /// **The point of the script.** After one run the trading signer is an
    /// executor, Slipstream's router is adapter 1 with `exactInputSingle`
    /// allowed, and the admin — not the trading signer — owns the router.
    function testDeploysAndWiresTheTradingSigner() external {
        (,, address clone, BatchRouter router) = script.runAll(_cfg(adminKey, admin), _wiring());
        MultiVenueArbImplementation exec = MultiVenueArbImplementation(clone);

        assertTrue(exec.executors(trader), "the trading signer may call startV2");
        assertEq(exec.adapterOf(script.SLIPSTREAM_ADAPTER_ID()), slipstream, "slipstream is adapter 1");
        assertTrue(
            exec.isSelectorAllowed(script.SLIPSTREAM_ADAPTER_ID(), script.SLIPSTREAM_EXACT_INPUT_SINGLE()),
            "exactInputSingle is allowed"
        );
        assertEq(router.owner(), admin, "the admin owns the router");
        assertEq(exec.owner(), address(router), "the router owns the executor");
        assertFalse(exec.configAdmins(trader), "the trading signer administers nothing");
    }

    /// The selector constant is Slipstream's struct — `int24 tickSpacing` — and
    /// not Uniswap's `uint24 fee`, which hashes differently.
    function testTheSelectorIsSlipstreams() external view {
        assertEq(
            script.SLIPSTREAM_EXACT_INPUT_SINGLE(),
            bytes4(keccak256("exactInputSingle((address,address,int24,address,uint256,uint256,uint256,uint160))"))
        );
        assertTrue(
            script.SLIPSTREAM_EXACT_INPUT_SINGLE()
                != bytes4(keccak256("exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))")),
            "that is Uniswap's"
        );
    }

    /// §18.1. `.env`'s `PRIVATE_KEY` is the trading key here, and `Deploy` would
    /// broadcast with it. This script refuses.
    function testRefusesTheTradingKeyAsDeployer() external {
        vm.expectRevert(abi.encodeWithSelector(DeployAndConfigure.TradingKeyMayNotDeploy.selector, trader));
        script.runAll(_cfg(traderKey, address(0)), _wiring());
    }

    function testRefusesTheTradingKeyAsOwner() external {
        vm.expectRevert(abi.encodeWithSelector(DeployAndConfigure.TradingKeyMayNotOwn.selector, trader));
        script.runAll(_cfg(adminKey, trader), _wiring());
    }

    /// An owner other than the broadcaster could not send the wiring in this run,
    /// so the deployment is refused before anything is created — rather than
    /// leaving an executor nobody can trade through.
    function testRefusesAnOwnerThatCannotConfigure() external {
        address elsewhere = makeAddr("elsewhere");
        vm.expectRevert(
            abi.encodeWithSelector(DeployAndConfigure.OwnerCannotConfigure.selector, elsewhere, admin)
        );
        script.runAll(_cfg(adminKey, elsewhere), _wiring());
    }

    function testRefusesMissingWiring() external {
        Deploy.Wiring memory w = _wiring();
        w.trader = address(0);
        vm.expectRevert(abi.encodeWithSelector(DeployAndConfigure.MissingWiring.selector, "BASE_EXECUTOR_TRADER"));
        script.runAll(_cfg(adminKey, admin), w);

        w = _wiring();
        w.slipstreamRouter = address(0);
        vm.expectRevert(abi.encodeWithSelector(DeployAndConfigure.MissingWiring.selector, "BASE_SLIPSTREAM_ROUTER"));
        script.runAll(_cfg(adminKey, admin), w);
    }
}
