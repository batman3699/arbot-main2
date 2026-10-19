// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {MockERC20} from "../../contracts/mocks/MockERC20.sol";
import {LeanExecutor} from "../../contracts/lean/LeanExecutor.sol";
import {DeployLeanExecutor} from "../../script/DeployLeanExecutor.s.sol";

/// Task 8.5 R29: the owner's deploy. The executor it leaves is the owner's,
/// trades only for the trader, pays the owner, and is the code it logs.
contract DeployLeanExecutorTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address owner = address(0xA11CE);
    address trader = address(0xB0B);

    function setUp() public {
        vm.chainId(8453);
        vm.etch(WETH, address(new MockERC20("Wrapped Ether", "WETH", 18)).code);
    }

    function testDeployScriptConfiguresTheExecutor() public {
        DeployLeanExecutor script = new DeployLeanExecutor();
        (LeanExecutor lean, bytes32 codeHash) = script.run(owner, trader);
        assertEq(lean.owner(), owner);
        assertTrue(lean.executors(trader));
        assertFalse(lean.executors(owner), "the owner does not trade");
        assertEq(lean.profitRecipient(), owner);
        assertFalse(lean.isCircuitOpen());
        assertEq(codeHash, keccak256(address(lean).code));
        assertEq(codeHash, address(lean).codehash);
    }

    function testTheDeployRefusesAnotherChain() public {
        vm.chainId(1);
        DeployLeanExecutor script = new DeployLeanExecutor();
        vm.expectRevert(bytes("Base only"));
        script.run(owner, trader);
    }
}
