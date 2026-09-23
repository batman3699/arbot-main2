// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {
    ProfitInvariant,
    DebtNotRepaid,
    UnaccountedResidue,
    ProfitTokenNotBorrowed,
    DuplicateDebtToken
} from "../contracts/core/ProfitInvariant.sol";
import {MockERC20} from "../contracts/mocks/MockERC20.sol";
import {Test} from "forge-std/Test.sol";

/// The library runs `balanceOf(address(this))`, so the checks have to be made
/// from a contract that holds the balances.
contract Settler {
    using ProfitInvariant for ProfitInvariant.Debt[];

    function check(ProfitInvariant.Debt[] memory debts, address profitToken)
        external
        view
        returns (uint256)
    {
        return ProfitInvariant.assertMultiAsset(debts, profitToken);
    }
}

contract ProfitInvariantTest is Test {
    Settler internal settler;
    MockERC20[4] internal tokens;

    function setUp() public {
        settler = new Settler();
        for (uint256 i; i < 4; ++i) {
            tokens[i] = new MockERC20("Tok", "TK", 18);
        }
    }

    function _debt(uint256 i, uint256 start, uint256 repay, uint256 residue)
        internal
        view
        returns (ProfitInvariant.Debt memory)
    {
        return ProfitInvariant.Debt({
            token: address(tokens[i]),
            startBalance: start,
            repayment: repay,
            declaredResidue: residue
        });
    }

    /// Every borrowed asset returns to at least its starting balance plus what
    /// is owed, for one through four assets and arbitrary amounts.
    function testMultiAssetInvariantHolds(
        uint8 rawCount,
        uint96 start,
        uint96 repay,
        uint96 profit
    ) external {
        uint256 count = (uint256(rawCount) % 4) + 1;
        ProfitInvariant.Debt[] memory debts = new ProfitInvariant.Debt[](count);

        for (uint256 i; i < count; ++i) {
            // Everything but the profit token ends exactly flat.
            uint256 surplus = i == 0 ? uint256(profit) : 0;
            tokens[i].mint(address(settler), uint256(start) + uint256(repay) + surplus);
            debts[i] = _debt(i, start, repay, 0);
        }

        uint256 realised = settler.check(debts, address(tokens[0]));
        assertEq(realised, uint256(profit), "profit is the surplus on the declared token");
    }

    /// One asset short is a revert naming that asset, not a generic failure.
    function testPartialRepaymentReverts() external {
        ProfitInvariant.Debt[] memory debts = new ProfitInvariant.Debt[](2);
        tokens[0].mint(address(settler), 100 ether);
        tokens[1].mint(address(settler), 99 ether); // one short
        debts[0] = _debt(0, 0, 100 ether, 0);
        debts[1] = _debt(1, 0, 100 ether, 0);

        vm.expectRevert(
            abi.encodeWithSelector(DebtNotRepaid.selector, address(tokens[1]), 99 ether, 100 ether)
        );
        settler.check(debts, address(tokens[0]));
    }

    /// **The multi-asset failure the single-asset check cannot see.**
    ///
    /// A plan ends holding far more of asset A and less of asset B. Checking A
    /// alone reads as a large profit. Netting the two would need a price, and
    /// the contract has none — so the answer is per-asset, and B's shortfall
    /// reverts however good A looks.
    function testAProfitOnOneAssetDoesNotExcuseAShortfallOnAnother() external {
        ProfitInvariant.Debt[] memory debts = new ProfitInvariant.Debt[](2);
        tokens[0].mint(address(settler), 1_000 ether); // hugely up
        tokens[1].mint(address(settler), 1 ether); //     badly down
        debts[0] = _debt(0, 0, 100 ether, 0);
        debts[1] = _debt(1, 0, 100 ether, 0);

        vm.expectRevert(
            abi.encodeWithSelector(DebtNotRepaid.selector, address(tokens[1]), 1 ether, 100 ether)
        );
        settler.check(debts, address(tokens[0]));
    }

    /// An intermediate token the plan did not expect to hold is a signal that
    /// a hop was mispriced or overtaken, not a windfall to pocket quietly.
    function testUnaccountedResidueReverts() external {
        ProfitInvariant.Debt[] memory debts = new ProfitInvariant.Debt[](2);
        tokens[0].mint(address(settler), 100 ether);
        tokens[1].mint(address(settler), 100 ether + 7 wei); // unexpected dust
        debts[0] = _debt(0, 0, 100 ether, 0);
        debts[1] = _debt(1, 0, 100 ether, 0); // declared: none

        vm.expectRevert(
            abi.encodeWithSelector(UnaccountedResidue.selector, address(tokens[1]), 7, 0)
        );
        settler.check(debts, address(tokens[0]));
    }

    /// A plan that declares what it expects to be left holding passes, and the
    /// declaration is exact rather than a ceiling nobody checks.
    function testDeclaredResiduePathAccountsExactly() external {
        ProfitInvariant.Debt[] memory debts = new ProfitInvariant.Debt[](2);
        tokens[0].mint(address(settler), 100 ether + 3 ether);
        tokens[1].mint(address(settler), 100 ether + 7 wei);
        debts[0] = _debt(0, 0, 100 ether, 0);
        debts[1] = _debt(1, 0, 100 ether, 7); // declared exactly

        assertEq(settler.check(debts, address(tokens[0])), 3 ether);

        // One wei more than declared still reverts: the declaration is a
        // statement about the route, not a tolerance band.
        tokens[1].mint(address(settler), 1 wei);
        vm.expectRevert(
            abi.encodeWithSelector(UnaccountedResidue.selector, address(tokens[1]), 8, 7)
        );
        settler.check(debts, address(tokens[0]));
    }

    /// Profit is measured against the PRE-loan balance. Measuring against the
    /// post-loan balance would read a pre-funded balance as profit the plan
    /// did not earn — a bug this repository has already had once.
    function testAPreFundedBalanceIsNotProfit() external {
        ProfitInvariant.Debt[] memory debts = new ProfitInvariant.Debt[](1);
        tokens[0].mint(address(settler), 500 ether); // dust that was already there
        tokens[0].mint(address(settler), 100 ether); // the loan
        debts[0] = _debt(0, 500 ether, 100 ether, 0);

        assertEq(settler.check(debts, address(tokens[0])), 0, "no profit was earned");
    }

    /// Profit in a token the plan never borrowed is profit measured against a
    /// balance nothing in the call accounted for.
    function testProfitTokenMustBeOneOfTheBorrowedAssets() external {
        ProfitInvariant.Debt[] memory debts = new ProfitInvariant.Debt[](1);
        tokens[0].mint(address(settler), 100 ether);
        debts[0] = _debt(0, 0, 100 ether, 0);

        vm.expectRevert(
            abi.encodeWithSelector(ProfitTokenNotBorrowed.selector, address(tokens[3]))
        );
        settler.check(debts, address(tokens[3]));
    }

    /// A token listed twice would be checked twice against one balance, and
    /// one entry's surplus could repay the other.
    function testADuplicateDebtTokenIsRefused() external {
        ProfitInvariant.Debt[] memory debts = new ProfitInvariant.Debt[](2);
        tokens[0].mint(address(settler), 150 ether);
        debts[0] = _debt(0, 0, 100 ether, 0);
        debts[1] = _debt(0, 0, 50 ether, 0);

        vm.expectRevert(
            abi.encodeWithSelector(DuplicateDebtToken.selector, address(tokens[0]))
        );
        settler.check(debts, address(tokens[0]));
    }

    /// The single-asset case still behaves, so the generalisation did not
    /// change the shape of what already worked.
    function testASingleAssetPlanIsUnchanged() external {
        ProfitInvariant.Debt[] memory debts = new ProfitInvariant.Debt[](1);
        tokens[0].mint(address(settler), 102 ether);
        debts[0] = _debt(0, 0, 100 ether, 0);
        assertEq(settler.check(debts, address(tokens[0])), 2 ether);
    }
}
