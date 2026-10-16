// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {MockERC20} from "../../contracts/mocks/MockERC20.sol";
import {LeanExecutor} from "../../contracts/lean/LeanExecutor.sol";

/// Task 8.5 R29: the lean executor's owner surface. The runtime reads three of
/// its views every block or at boot, so their signatures are the old
/// executor's; everything that changes it is the owner's alone.
contract LeanExecutorAdminTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant MORPHO_BLUE = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;

    address owner = address(0xA11CE);
    address trader = address(0xB0B);
    address recipient = address(0xCAFE);
    LeanExecutor lean;

    function setUp() public {
        vm.etch(WETH, address(new MockERC20("Wrapped Ether", "WETH", 18)).code);
        lean = new LeanExecutor(owner, trader, recipient);
    }

    function testConstructorSetsRoles() public view {
        assertEq(lean.owner(), owner);
        assertTrue(lean.executors(trader));
        assertFalse(lean.executors(owner), "the owner is not a trader unless made one");
        assertEq(lean.profitRecipient(), recipient);
        assertFalse(lean.isCircuitOpen());
        assertEq(MockERC20(WETH).allowance(address(lean), MORPHO_BLUE), type(uint256).max, "Morpho pulls its repayment");
    }

    function testZeroAddressesAreRefused() public {
        vm.expectRevert(LeanExecutor.ZeroAddress.selector);
        new LeanExecutor(address(0), trader, recipient);
        vm.expectRevert(LeanExecutor.ZeroAddress.selector);
        new LeanExecutor(owner, address(0), recipient);
        vm.expectRevert(LeanExecutor.ZeroAddress.selector);
        new LeanExecutor(owner, trader, address(0));
        vm.startPrank(owner);
        vm.expectRevert(LeanExecutor.ZeroAddress.selector);
        lean.setProfitRecipient(address(0));
        vm.expectRevert(LeanExecutor.ZeroAddress.selector);
        lean.transferOwnership(address(0));
        vm.expectRevert(LeanExecutor.ZeroAddress.selector);
        lean.setExecutor(address(0), true);
        vm.stopPrank();
    }

    function testEveryAdminCallIsOwnerOnly() public {
        vm.startPrank(trader);
        vm.expectRevert(LeanExecutor.NotOwner.selector);
        lean.setExecutor(trader, false);
        vm.expectRevert(LeanExecutor.NotOwner.selector);
        lean.setProfitRecipient(trader);
        vm.expectRevert(LeanExecutor.NotOwner.selector);
        lean.tripCircuit();
        vm.expectRevert(LeanExecutor.NotOwner.selector);
        lean.resetCircuit();
        vm.expectRevert(LeanExecutor.NotOwner.selector);
        lean.transferOwnership(trader);
        vm.expectRevert(LeanExecutor.NotOwner.selector);
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
