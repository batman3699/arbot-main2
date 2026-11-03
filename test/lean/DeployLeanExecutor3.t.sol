// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {MockERC20} from "../../contracts/mocks/MockERC20.sol";
import {LeanExecutor3} from "../../contracts/lean/LeanExecutor3.sol";
import {DeployLeanExecutor3} from "../../script/DeployLeanExecutor3.s.sol";

/// Task 8.5 R40.1: the owner's deploy of the third lean executor. It is the
/// owner's, trades only for the trader, pays the owner, lets Morpho pull its
/// repayment in either start token, and is the code it logs.
contract DeployLeanExecutor3Test is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant MORPHO_BLUE = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    address owner = address(0xA11CE);
    address trader = address(0xB0B);

    function setUp() public {
        vm.chainId(8453);
        vm.etch(WETH, address(new MockERC20("Wrapped Ether", "WETH", 18)).code);
        vm.etch(USDC, address(new MockERC20("USD Coin", "USDC", 6)).code);
    }

    function testDeployScriptConfiguresTheExecutor() public {
        DeployLeanExecutor3 script = new DeployLeanExecutor3();
        (LeanExecutor3 lean, bytes32 codeHash) = script.run(owner, trader);
        assertEq(lean.owner(), owner);
        assertTrue(lean.executors(trader));
        assertFalse(lean.executors(owner), "the owner does not trade");
        assertEq(lean.profitRecipient(), owner);
        assertFalse(lean.isCircuitOpen());
        assertEq(MockERC20(WETH).allowance(address(lean), MORPHO_BLUE), type(uint256).max);
        assertEq(MockERC20(USDC).allowance(address(lean), MORPHO_BLUE), type(uint256).max, "Morpho pulls USDC too");
        assertEq(codeHash, keccak256(address(lean).code));
        assertEq(codeHash, address(lean).codehash);
    }

    function testTheDeployRefusesAnotherChain() public {
        vm.chainId(1);
        DeployLeanExecutor3 script = new DeployLeanExecutor3();
        vm.expectRevert(bytes("Base only"));
        script.run(owner, trader);
    }
}
