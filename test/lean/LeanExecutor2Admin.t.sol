// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {MockERC20} from "../../contracts/mocks/MockERC20.sol";
import {LeanExecutor2} from "../../contracts/lean/LeanExecutor2.sol";

/// Task 8.5 R37: the second lean executor's owner surface, v1's unchanged. The
/// runtime reads three of its views every block or at boot, so their
/// signatures are v1's; everything that changes it is the owner's alone.
contract LeanExecutor2AdminTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant MORPHO_BLUE = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;

    address owner = address(0xA11CE);
    address trader = address(0xB0B);
    address recipient = address(0xCAFE);
    LeanExecutor2 lean;

    function setUp() public {
        vm.etch(WETH, address(new MockERC20("Wrapped Ether", "WETH", 18)).code);
        vm.etch(USDC, address(new MockERC20("USD Coin", "USDC", 6)).code);
        lean = new LeanExecutor2(owner, trader, recipient);
    }

    function testConstructorSetsRoles() public view {
        assertEq(lean.owner(), owner);
        assertTrue(lean.executors(trader));
        assertFalse(lean.executors(owner), "the owner is not a trader unless made one");
        assertEq(lean.profitRecipient(), recipient);
        assertFalse(lean.isCircuitOpen());
        assertEq(MockERC20(WETH).allowance(address(lean), MORPHO_BLUE), type(uint256).max, "Morpho pulls its repayment");
        assertEq(MockERC20(USDC).allowance(address(lean), MORPHO_BLUE), type(uint256).max, "in either start token");
    }

    function testZeroAddressesAreRefused() public {
        vm.expectRevert(LeanExecutor2.ZeroAddress.selector);
        new LeanExecutor2(address(0), trader, recipient);
        vm.expectRevert(LeanExecutor2.ZeroAddress.selector);
        new LeanExecutor2(owner, address(0), recipient);
        vm.expectRevert(LeanExecutor2.ZeroAddress.selector);
        new LeanExecutor2(owner, trader, address(0));
        vm.startPrank(owner);
        vm.expectRevert(LeanExecutor2.ZeroAddress.selector);
        lean.setProfitRecipient(address(0));
        vm.expectRevert(LeanExecutor2.ZeroAddress.selector);
        lean.transferOwnership(address(0));
        vm.expectRevert(LeanExecutor2.ZeroAddress.selector);
        lean.setExecutor(address(0), true);
        vm.stopPrank();
    }

    function testEveryAdminCallIsOwnerOnly() public {
        vm.startPrank(trader);
        vm.expectRevert(LeanExecutor2.NotOwner.selector);
        lean.setExecutor(trader, false);
        vm.expectRevert(LeanExecutor2.NotOwner.selector);
        lean.setProfitRecipient(trader);
        vm.expectRevert(LeanExecutor2.NotOwner.selector);
        lean.tripCircuit();
        vm.expectRevert(LeanExecutor2.NotOwner.selector);
        lean.resetCircuit();
        vm.expectRevert(LeanExecutor2.NotOwner.selector);
        lean.transferOwnership(trader);
        vm.expectRevert(LeanExecutor2.NotOwner.selector);
        lean.sweep(WETH, trader, 1);
        vm.stopPrank();
    }

    function testTheOwnerChangesRolesAndRecipient() public {
        vm.startPrank(owner);
        lean.setExecutor(trader, false);
        lean.setProfitRecipient(owner);
        lean.transferOwnership(trader);
        vm.stopPrank();
        assertFalse(lean.executors(trader));
        assertEq(lean.profitRecipient(), owner);
        assertEq(lean.owner(), trader);
    }

    function testCircuitTripsAndResets() public {
        vm.prank(owner);
        lean.tripCircuit();
        assertTrue(lean.isCircuitOpen());
        assertEq(lean.profitRecipient(), recipient, "sharing a slot, the recipient is untouched");
        vm.prank(owner);
        lean.resetCircuit();
        assertFalse(lean.isCircuitOpen());
    }

    function testSweepMovesTokens() public {
        MockERC20(WETH).mint(address(lean), 5);
        vm.prank(owner);
        lean.sweep(WETH, owner, 3);
        assertEq(MockERC20(WETH).balanceOf(owner), 3);
        assertEq(MockERC20(WETH).balanceOf(address(lean)), 2);
    }

    function testTheReaderSelectorsAreUnchanged() public view {
        assertEq(lean.executors.selector, bytes4(keccak256("executors(address)")));
        assertEq(lean.isCircuitOpen.selector, bytes4(keccak256("isCircuitOpen()")));
        assertEq(lean.profitRecipient.selector, bytes4(keccak256("profitRecipient()")));
    }
}
