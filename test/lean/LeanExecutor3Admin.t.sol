// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {MockERC20} from "../../contracts/mocks/MockERC20.sol";
import {LeanExecutor3} from "../../contracts/lean/LeanExecutor3.sol";

/// Task 8.5 R40.1: the third lean executor's owner surface: v2's, and a batch
/// authorization. The runtime reads three of its views every block or at boot,
/// so their signatures are v1's; everything that changes it is the owner's.
contract LeanExecutor3AdminTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant MORPHO_BLUE = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;

    address owner = address(0xA11CE);
    address trader = address(0xB0B);
    address recipient = address(0xCAFE);
    LeanExecutor3 lean;

    function setUp() public {
        vm.etch(WETH, address(new MockERC20("Wrapped Ether", "WETH", 18)).code);
        vm.etch(USDC, address(new MockERC20("USD Coin", "USDC", 6)).code);
        lean = new LeanExecutor3(owner, trader, recipient);
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
        vm.expectRevert(LeanExecutor3.ZeroAddress.selector);
        new LeanExecutor3(address(0), trader, recipient);
        vm.expectRevert(LeanExecutor3.ZeroAddress.selector);
        new LeanExecutor3(owner, address(0), recipient);
        vm.expectRevert(LeanExecutor3.ZeroAddress.selector);
        new LeanExecutor3(owner, trader, address(0));
        vm.startPrank(owner);
        vm.expectRevert(LeanExecutor3.ZeroAddress.selector);
        lean.setProfitRecipient(address(0));
        vm.expectRevert(LeanExecutor3.ZeroAddress.selector);
        lean.transferOwnership(address(0));
        vm.expectRevert(LeanExecutor3.ZeroAddress.selector);
        lean.setExecutor(address(0), true);
        vm.stopPrank();
    }

    function testEveryAdminCallIsOwnerOnly() public {
        vm.startPrank(trader);
        vm.expectRevert(LeanExecutor3.NotOwner.selector);
        lean.setExecutor(trader, false);
        vm.expectRevert(LeanExecutor3.NotOwner.selector);
        lean.setProfitRecipient(trader);
        vm.expectRevert(LeanExecutor3.NotOwner.selector);
        lean.tripCircuit();
        vm.expectRevert(LeanExecutor3.NotOwner.selector);
        lean.resetCircuit();
        vm.expectRevert(LeanExecutor3.NotOwner.selector);
        lean.transferOwnership(trader);
        vm.expectRevert(LeanExecutor3.NotOwner.selector);
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

    function testSetExecutorsAuthorizesAndRevokesTheWholeSet() public {
        address[] memory senders = new address[](3);
        (senders[0], senders[1], senders[2]) = (address(0x51), address(0x52), address(0x53));
        vm.prank(owner);
        lean.setExecutors(senders, true);
        for (uint256 i; i < 3; i++) {
            assertTrue(lean.executors(senders[i]));
        }
        vm.prank(owner);
        lean.setExecutors(senders, false);
        for (uint256 i; i < 3; i++) {
            assertFalse(lean.executors(senders[i]));
        }
        assertTrue(lean.executors(trader), "the constructor's trader is untouched");
    }

    function testSetExecutorsIsOwnerOnlyAndRefusesZero() public {
        address[] memory senders = new address[](2);
        senders[0] = address(0x51);
        vm.expectRevert(LeanExecutor3.NotOwner.selector);
        vm.prank(trader);
        lean.setExecutors(senders, true);
        vm.expectRevert(LeanExecutor3.ZeroAddress.selector);
        vm.prank(owner);
        lean.setExecutors(senders, true);
        assertFalse(lean.executors(address(0x51)), "nothing applied");
    }

    function testTheReaderSelectorsAreUnchanged() public view {
        assertEq(lean.executors.selector, bytes4(keccak256("executors(address)")));
        assertEq(lean.isCircuitOpen.selector, bytes4(keccak256("isCircuitOpen()")));
        assertEq(lean.profitRecipient.selector, bytes4(keccak256("profitRecipient()")));
    }
}
