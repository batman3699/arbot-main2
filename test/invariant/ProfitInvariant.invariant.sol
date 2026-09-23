// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {ProfitInvariant} from "../../contracts/core/ProfitInvariant.sol";
import {MockERC20} from "../../contracts/mocks/MockERC20.sol";
import {Test} from "forge-std/Test.sol";

/// Holds the balances the invariant is checked against, and lets the fuzzer
/// move them.
contract Settler {
    MockERC20[3] public tokens;

    /// What each token's balance was before the loans arrived.
    uint256[3] public startBalance;
    /// What is owed on each.
    uint256[3] public repayment;
    /// What the plan said it expects to be left holding.
    uint256[3] public declaredResidue;

    constructor() {
        for (uint256 i; i < 3; ++i) {
            tokens[i] = new MockERC20("Tok", "TK", 18);
        }
    }

    // ---- the operations the fuzzer may perform -----------------------------

    /// A step that brings value in.
    function gain(uint8 which, uint96 amount) external {
        tokens[which % 3].mint(address(this), amount);
    }

    /// A step that sends value out. Bounded by the balance, because a step
    /// that tries to send more than the contract holds reverts inside the
    /// token and tells the invariant nothing.
    function lose(uint8 which, uint96 amount) external {
        MockERC20 token = tokens[which % 3];
        uint256 balance = token.balanceOf(address(this));
        if (balance == 0) return;
        token.transfer(address(0xDEAD), uint256(amount) % balance);
    }

    function setDebt(uint8 which, uint96 start, uint96 repay, uint96 residue) external {
        uint256 i = which % 3;
        startBalance[i] = start;
        repayment[i] = repay;
        declaredResidue[i] = residue;
    }

    // ---- the thing under test ---------------------------------------------

    function debts(uint256 count) public view returns (ProfitInvariant.Debt[] memory out) {
        out = new ProfitInvariant.Debt[](count);
        for (uint256 i; i < count; ++i) {
            out[i] = ProfitInvariant.Debt({
                token: address(tokens[i]),
                startBalance: startBalance[i],
                repayment: repayment[i],
                declaredResidue: declaredResidue[i]
            });
        }
    }

    function check(uint256 count) external view returns (uint256) {
        return ProfitInvariant.assertMultiAsset(debts(count), address(tokens[0]));
    }

    function balanceOf(uint256 i) external view returns (uint256) {
        return tokens[i].balanceOf(address(this));
    }
}

/// Task 5.6 — `ProfitInvariant` under arbitrary balance movements.
///
/// The property is a **postcondition**, which is the useful shape for a
/// settlement check: whenever `assertMultiAsset` returns rather than reverting,
/// the state it returned on must actually satisfy the invariant. A stateful
/// fuzz suite is how that gets checked against sequences nobody wrote down —
/// the fuzzer moves balances around and every invariant call re-asks the
/// question against whatever state it produced.
contract ProfitInvariantInvariant is Test {
    Settler internal settler;

    function setUp() public {
        settler = new Settler();
        targetContract(address(settler));

        bytes4[] memory selectors = new bytes4[](3);
        selectors[0] = Settler.gain.selector;
        selectors[1] = Settler.lose.selector;
        selectors[2] = Settler.setDebt.selector;
        targetSelector(FuzzSelector({addr: address(settler), selectors: selectors}));
    }

    /// If the check passes, every debt really was covered.
    ///
    /// The direction matters: this does not claim the check accepts everything
    /// it should. It claims it never accepts anything it should not — which is
    /// the only direction a settlement invariant may be wrong in, because a
    /// false rejection loses a trade and a false acceptance loses the money.
    function invariant_passingImpliesEveryDebtIsCovered() public view {
        for (uint256 count = 1; count <= 3; ++count) {
            try settler.check(count) returns (uint256 profit) {
                for (uint256 i; i < count; ++i) {
                    uint256 balance = settler.balanceOf(i);
                    uint256 required = settler.startBalance(i) + settler.repayment(i);
                    assertGe(balance, required, "the check passed on an uncovered debt");

                    uint256 surplus = balance - required;
                    if (i == 0) {
                        assertEq(profit, surplus, "the reported profit is not the surplus");
                    } else {
                        assertLe(
                            surplus,
                            settler.declaredResidue(i),
                            "the check passed on an undeclared residue"
                        );
                    }
                }
            } catch {
                // A rejection is always permissible; this invariant is only
                // about what an acceptance implies.
            }
        }
    }

    /// The profit reported is never more than the contract actually holds
    /// above what it owes. A check that overstated profit would let
    /// `minProfit` pass on money that is not there.
    function invariant_reportedProfitIsBackedByBalance() public view {
        try settler.check(1) returns (uint256 profit) {
            uint256 balance = settler.balanceOf(0);
            assertLe(profit, balance, "profit exceeds the balance backing it");
        } catch {}
    }

    /// Nothing the fuzzer can do makes the check pass with a debt still
    /// outstanding on the profit token itself — the asset the trade is
    /// denominated in is the one most likely to be double-counted.
    function invariant_theProfitTokenIsNeverShort() public view {
        try settler.check(3) returns (uint256) {
            assertGe(
                settler.balanceOf(0),
                settler.startBalance(0) + settler.repayment(0),
                "the profit token was short and the check passed"
            );
        } catch {}
    }
}
