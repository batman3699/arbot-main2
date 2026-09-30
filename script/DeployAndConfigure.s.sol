// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Deploy} from "./Deploy.s.sol";
import {MultiVenueArbImplementation} from "../contracts/executor/MultiVenueArbImplementation.sol";
import {ArbitrageCloneFactory} from "../contracts/executor/ArbitrageCloneFactory.sol";
import {BatchRouter} from "../contracts/executor/BatchRouter.sol";
import "forge-std/console2.sol";

/// The Phase 5 executor, deployed **and** wired for the signer, in one broadcast.
///
/// # Why one script, and why it refuses the trading key
///
/// `Deploy` leaves the clone owned by its `BatchRouter`, with the router as the
/// only executor. The trading signer then has to be added with `setExecutor`, and
/// Aerodrome Slipstream's router registered as a `GENERIC` adapter target — both
/// owner-only, so both go through `router.multicall`. Doing it in the same run
/// means `forge script` simulates **every** transaction before it broadcasts any:
/// a configuration that would revert stops the deployment before it starts,
/// rather than leaving a deployed executor nobody can trade through.
///
/// §18.1: "No general-purpose wallet holds unrestricted operational authority."
/// The trading signer signs arbitrage and holds gas; it must not deploy or own the
/// executor. `Deploy` broadcasts with `PRIVATE_KEY`, forge auto-loads `.env`, and
/// in this repository `PRIVATE_KEY` is the trading key — so running `Deploy`
/// naively makes the trading key the deployer. This script refuses that instead
/// of relying on the operator to remember it.
///
/// # Inputs, read once
///
/// Everything `Deploy.configFromEnv` reads, plus `BASE_EXECUTOR_TRADER` (the
/// trading signer's address) and `BASE_SLIPSTREAM_ROUTER`, which
/// `Deploy.wiringFromEnv` reads — `Deploy.s.sol` stays the only env reader (B-13). Run with
/// `PRIVATE_KEY=` and `--account <admin>` (or `--ledger`): an empty `PRIVATE_KEY`
/// makes `Deploy` broadcast as the CLI's wallet, and the owner must be that
/// wallet so the configuration can be sent in the same run.
contract DeployAndConfigure is Deploy {
    /// The adapter id `apex-exec`'s call builder uses for Slipstream hops. A
    /// constant here and there, and `test/DeployAndConfigure.t.sol` asserts the
    /// registered target answers to it.
    uint16 public constant SLIPSTREAM_ADAPTER_ID = 1;

    /// `exactInputSingle((address,address,int24,address,uint256,uint256,uint256,uint160))`
    /// on Aerodrome Slipstream's `SwapRouter` — Slipstream's own struct, with
    /// `int24 tickSpacing` where Uniswap has `uint24 fee`. Verified present in the
    /// deployed router's bytecode on Base, 2026-09-30.
    bytes4 public constant SLIPSTREAM_EXACT_INPUT_SINGLE = 0xa026383e;

    error TradingKeyMayNotDeploy(address trader);
    error TradingKeyMayNotOwn(address trader);
    error MissingWiring(string what);
    error OwnerCannotConfigure(address owner, address broadcaster);

    function run()
        external
        override
        returns (MultiVenueArbImplementation impl, ArbitrageCloneFactory factory, address clone, BatchRouter router)
    {
        return runAll(configFromEnv(bytes32(0)), wiringFromEnv());
    }

    /// The configuration calls, as data. Pure so a test can inspect exactly what
    /// the owner will be asked to send.
    function wiringCalls(address clone, Wiring memory w)
        public
        pure
        returns (address[] memory targets, bytes[] memory data)
    {
        targets = new address[](3);
        data = new bytes[](3);
        targets[0] = clone;
        data[0] = abi.encodeCall(MultiVenueArbImplementation.setExecutor, (w.trader, true));
        targets[1] = clone;
        data[1] = abi.encodeCall(
            MultiVenueArbImplementation.registerAdapter, (SLIPSTREAM_ADAPTER_ID, w.slipstreamRouter)
        );
        targets[2] = clone;
        data[2] = abi.encodeCall(
            MultiVenueArbImplementation.allowSelector, (SLIPSTREAM_ADAPTER_ID, SLIPSTREAM_EXACT_INPUT_SINGLE)
        );
    }

    function runAll(DeployConfig memory cfg, Wiring memory w)
        public
        returns (MultiVenueArbImplementation impl, ArbitrageCloneFactory factory, address clone, BatchRouter router)
    {
        if (w.trader == address(0)) revert MissingWiring("BASE_EXECUTOR_TRADER");
        if (w.slipstreamRouter == address(0)) revert MissingWiring("BASE_SLIPSTREAM_ROUTER");

        address broadcaster = cfg.privateKey != 0 ? vm.addr(cfg.privateKey) : msg.sender;
        if (broadcaster == w.trader) revert TradingKeyMayNotDeploy(w.trader);
        if (cfg.configuredOwner == w.trader) revert TradingKeyMayNotOwn(w.trader);
        // The configuration is owner-only and goes out in this run, so the owner
        // has to be the broadcaster. Checked before anything is deployed.
        if (cfg.configuredOwner != address(0) && cfg.configuredOwner != broadcaster) {
            revert OwnerCannotConfigure(cfg.configuredOwner, broadcaster);
        }

        (impl, factory, clone, router) = runWith(cfg);

        (address[] memory targets, bytes[] memory data) = wiringCalls(clone, w);
        if (cfg.privateKey != 0) {
            vm.startBroadcast(cfg.privateKey);
        } else {
            vm.startBroadcast();
        }
        router.multicall(targets, data);
        vm.stopBroadcast();

        console2.log("Trading signer authorized", w.trader);
        console2.log("Slipstream router registered as adapter", SLIPSTREAM_ADAPTER_ID, w.slipstreamRouter);
    }
}
