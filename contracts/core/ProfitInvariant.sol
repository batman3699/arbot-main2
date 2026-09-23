// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

error DebtNotRepaid(address token, uint256 finalBalance, uint256 required);
error UnaccountedResidue(address token, uint256 surplus, uint256 declared);
error ProfitTokenNotBorrowed(address profitToken);
error DuplicateDebtToken(address token);

interface IERC20Balance {
    function balanceOf(address account) external view returns (uint256);
}

/// The settlement invariant for a plan that borrows more than one asset
/// (INV-27, INV-29).
///
/// # A multi-asset invariant is not N copies of the single-asset one
///
/// With one borrowed asset, "ended holding more than we started" is the whole
/// question. With several it is not, and the gap is where the money goes.
///
/// A plan can end holding **more** of asset A and **less** of asset B and look
/// profitable if you only check A. Netting the two requires a price, and the
/// contract has none — it cannot know whether 3 units of A are worth more than
/// 2 of B, and any rate it used would be one the plan supplied, which is the
/// same class of mistake as letting the plan supply a call target.
///
/// So the invariant is **per asset**, and profit is measured in exactly one
/// declared token. Every borrowed asset must return to at least its starting
/// balance plus what is owed; the surplus on the profit token is the profit;
/// a surplus on any other token is residue.
///
/// # Residue is a signal, not a windfall
///
/// A route that ends holding an intermediate token it did not expect has
/// either mispriced a hop or been overtaken mid-route. Keeping it quietly lets
/// the contract's balance sheet drift away from what the planner believes, and
/// the next plan prices against a balance nobody accounted for.
///
/// So a surplus above what the plan **declared** it expects to hold reverts.
/// Declaring zero is the normal case and says "this route should end flat in
/// everything but the profit token".
library ProfitInvariant {
    struct Debt {
        address token;
        /// What the contract held before the loan arrived. Profit is measured
        /// against this, never against the post-loan balance — otherwise a
        /// pre-funded balance reads as profit the plan did not earn.
        uint256 startBalance;
        /// Principal plus fee.
        uint256 repayment;
        /// How much of this token the plan expects to be left holding beyond
        /// its start balance. Zero for every token on a route that ends flat.
        uint256 declaredResidue;
    }

    /// Check every debt and return the profit, denominated in `profitToken`.
    ///
    /// Reverts naming the specific token and the specific shortfall, so a
    /// failure says which asset broke rather than that something did.
    function assertMultiAsset(Debt[] memory debts, address profitToken)
        internal
        view
        returns (uint256 profit)
    {
        uint256 len = debts.length;
        bool sawProfitToken;

        for (uint256 i; i < len;) {
            Debt memory debt = debts[i];

            // A token listed twice would be checked twice against one balance
            // and could repay one entry with the other's surplus.
            for (uint256 j; j < i;) {
                if (debts[j].token == debt.token) revert DuplicateDebtToken(debt.token);
                unchecked { ++j; }
            }

            uint256 balance = IERC20Balance(debt.token).balanceOf(address(this));
            uint256 required = debt.startBalance + debt.repayment;
            if (balance < required) {
                revert DebtNotRepaid(debt.token, balance, required);
            }

            uint256 surplus = balance - required;
            if (debt.token == profitToken) {
                sawProfitToken = true;
                profit = surplus;
            } else if (surplus > debt.declaredResidue) {
                revert UnaccountedResidue(debt.token, surplus, debt.declaredResidue);
            }

            unchecked { ++i; }
        }

        // Profit in a token the plan never borrowed is profit measured against
        // a balance nothing in this call accounted for.
        if (!sawProfitToken) revert ProfitTokenNotBorrowed(profitToken);
    }
}
