// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {MultiVenueArbImplementation, NotExecutor} from "../contracts/executor/MultiVenueArbImplementation.sol";
import {ArbitrageCloneFactory} from "../contracts/executor/ArbitrageCloneFactory.sol";
import {SelectorNotAllowed, EmptyCallData, UnknownAdapter} from "../contracts/core/AdapterRegistry.sol";
import {DebtNotRepaid} from "../contracts/core/ProfitInvariant.sol";
import {MockERC20} from "../contracts/mocks/MockERC20.sol";
import {MockERC3156Lender} from "../contracts/mocks/MockERC3156Lender.sol";
import {Test} from "forge-std/Test.sol";

contract PermitStub {}

/// A registered adapter that also exposes a function nobody allowlisted --
/// which is the normal state of any real venue contract.
contract Donor {
    function donate(address token, address to, uint256 amount) external {
        MockERC20(token).transfer(to, amount);
    }

    /// The admin surface an adapter happens to carry. Never allowlisted.
    function sweep(address token, address to) external {
        MockERC20(token).transfer(to, MockERC20(token).balanceOf(address(this)));
    }
}

/// §8.4's settlement invariants, one named test each (Task 5.4).
///
/// INV-25 (approved callback sender), INV-27 and INV-29 (per-asset repayment
/// and residue) and INV-33 (no arbitrary call) are covered by existing suites
/// and are not duplicated here. What this file adds is the rest: INV-24,
/// INV-26's selector half, INV-28, INV-30, INV-31 and INV-32.
contract SettlementInvariantsTest is Test {
    uint16 internal constant DONOR_ID = 1;

    MultiVenueArbImplementation internal executor;
    MockERC3156Lender internal lender;
    MockERC20 internal loanToken;
    Donor internal donor;

    function setUp() public {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        executor = MultiVenueArbImplementation(factory.deployClone(bytes32("inv")));

        loanToken = new MockERC20("Loan", "LN", 18);
        lender = new MockERC3156Lender(address(loanToken), 0);
        donor = new Donor();
        executor.initialise(address(this), address(1), address(1), address(0), address(new PermitStub()), 0, 0, 1);

        loanToken.mint(address(lender), 1_000 ether);
        loanToken.mint(address(donor), 100 ether);
        executor.registerAdapter(DONOR_ID, address(donor));
        executor.allowSelector(DONOR_ID, Donor.donate.selector);
    }

    function _step(bytes memory callData)
        internal
        pure
        returns (MultiVenueArbImplementation.Step[] memory steps)
    {
        steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.GENERIC,
            data: abi.encode(DONOR_ID, address(0), uint256(0), callData)
        });
    }

    function _donateStep(uint256 amount)
        internal
        view
        returns (MultiVenueArbImplementation.Step[] memory)
    {
        return _step(abi.encodeCall(Donor.donate, (address(loanToken), address(executor), amount)));
    }

    function _plan(MultiVenueArbImplementation.Step[] memory steps, uint256 minProfit)
        internal
        view
        returns (MultiVenueArbImplementation.PlanV2 memory plan)
    {
        MultiVenueArbImplementation.Loan[] memory loans = new MultiVenueArbImplementation.Loan[](1);
        loans[0] = MultiVenueArbImplementation.Loan({
            token: address(loanToken),
            amount: 1 ether,
            provider: MultiVenueArbImplementation.LoanProvider.ERC3156,
            providerAddr: address(lender)
        });
        plan = MultiVenueArbImplementation.PlanV2({
            loans: loans,
            cycleSlippageBps: 0,
            steps: steps,
            minProfit: minProfit,
            declaredResidue: 0,
            commitment: bytes32(0),
            chainId: 0,
            deadline: 0
        });
    }

    /// INV-24 — only the authorised caller can execute.
    function testOnlyExecutorCanStart() external {
        MultiVenueArbImplementation.PlanV2 memory plan = _plan(_donateStep(2 ether), 0);

        vm.prank(address(0xDEAD));
        vm.expectRevert(NotExecutor.selector);
        executor.startV2(plan);

        // ...and the authorised caller still can, so the test is about
        // authority rather than about the plan being broken.
        assertEq(executor.startV2(plan), 2 ether);
    }

    /// INV-26 — a registered adapter's *other* functions are not reachable.
    ///
    /// Resolving the address stops a plan calling anywhere. It does not stop a
    /// plan calling an adapter's admin surface, its upgrade hook, or a
    /// `transfer` it happens to expose — and every real venue contract has
    /// more functions than the one a route needs.
    function testUnlistedSelectorReverts() external {
        MultiVenueArbImplementation.PlanV2 memory plan =
            _plan(_step(abi.encodeCall(Donor.sweep, (address(loanToken), address(0xBEEF)))), 0);

        vm.expectRevert(
            abi.encodeWithSelector(SelectorNotAllowed.selector, DONOR_ID, Donor.sweep.selector)
        );
        executor.startV2(plan);
    }

    /// INV-26 — an unregistered adapter is unreachable whatever the selector.
    function testUnlistedTargetReverts() external {
        MultiVenueArbImplementation.Step[] memory steps =
            new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.GENERIC,
            data: abi.encode(
                uint16(777),
                address(0),
                uint256(0),
                abi.encodeCall(Donor.donate, (address(loanToken), address(executor), 1))
            )
        });

        vm.expectRevert(abi.encodeWithSelector(UnknownAdapter.selector, uint16(777)));
        executor.startV2(_plan(steps, 0));
    }

    /// INV-26 — empty calldata is refused outright.
    ///
    /// It invokes the adapter's `receive` or `fallback`: a function nobody
    /// allowlisted, and one a selector check cannot see because there is no
    /// selector to check.
    function testEmptyCallDataReverts() external {
        vm.expectRevert(EmptyCallData.selector);
        executor.startV2(_plan(_step(bytes("")), 0));

        // Three bytes is not a selector either.
        vm.expectRevert(EmptyCallData.selector);
        executor.startV2(_plan(_step(hex"aabbcc"), 0));
    }

    /// INV-26 — revoking a selector takes effect.
    function testARevokedSelectorStopsWorking() external {
        assertEq(executor.startV2(_plan(_donateStep(2 ether), 0)), 2 ether);

        executor.revokeSelector(DONOR_ID, Donor.donate.selector);
        vm.expectRevert(
            abi.encodeWithSelector(SelectorNotAllowed.selector, DONOR_ID, Donor.donate.selector)
        );
        executor.startV2(_plan(_donateStep(2 ether), 0));
    }

    /// INV-28 — profit below the declared minimum reverts.
    function testProfitBelowMinimumReverts() external {
        // Earns 1 wei, demands 2 ether.
        vm.expectRevert();
        executor.startV2(_plan(_donateStep(1), 2 ether));

        // The same plan clears a minimum it actually meets.
        assertEq(executor.startV2(_plan(_donateStep(2 ether), 2 ether)), 2 ether);
    }

    /// INV-30 — a plan built for another chain does not execute here.
    function testWrongChainReverts() external {
        MultiVenueArbImplementation.PlanV2 memory plan = _plan(_donateStep(2 ether), 0);
        plan.chainId = 1; // Ethereum, while this is Foundry's default

        vm.expectRevert(
            abi.encodeWithSelector(
                MultiVenueArbImplementation.WrongChain.selector, uint64(1), block.chainid
            )
        );
        executor.startV2(plan);

        // Stating the right chain executes, so the check is about the value
        // rather than about the field being present.
        plan.chainId = uint64(block.chainid);
        assertEq(executor.startV2(plan), 2 ether);
    }

    /// INV-31 — a route past its validity window reverts.
    ///
    /// The contract had no plan deadline before this: it derived one from
    /// `block.timestamp` at execution, which is a fresh clock every time
    /// rather than an expiry. A plan that sat in a queue for two minutes
    /// executed against prices from two minutes ago with nothing to object.
    function testExpiredDeadlineReverts() external {
        MultiVenueArbImplementation.PlanV2 memory plan = _plan(_donateStep(2 ether), 0);
        plan.deadline = uint64(block.timestamp + 60);

        vm.warp(block.timestamp + 61);
        vm.expectRevert(
            abi.encodeWithSelector(
                MultiVenueArbImplementation.RouteExpired.selector,
                plan.deadline,
                block.timestamp
            )
        );
        executor.startV2(plan);
    }

    /// ...and the boundary is inclusive: a plan is valid *at* its deadline.
    function testAPlanIsValidAtItsDeadline() external {
        MultiVenueArbImplementation.PlanV2 memory plan = _plan(_donateStep(2 ether), 0);
        plan.deadline = uint64(block.timestamp + 60);
        vm.warp(plan.deadline);
        assertEq(executor.startV2(plan), 2 ether);
    }

    /// INV-32 — a loan that cannot be repaid takes the whole transaction with
    /// it. There is no success exit that leaves a debt behind.
    ///
    /// The fee is what makes this a shortfall. A **zero-fee** flash loan
    /// borrowed and returned untouched satisfies the invariant trivially --
    /// the executor holds exactly what it owes -- which is correct and is why
    /// the first version of this test did not revert. A fee means a no-op plan
    /// is short by precisely the fee, which is the smallest honest shortfall
    /// available.
    function testRepaymentShortfallReverts() external {
        uint256 feeBps = 50;
        MockERC3156Lender feeLender = new MockERC3156Lender(address(loanToken), feeBps);
        loanToken.mint(address(feeLender), 100 ether);

        MultiVenueArbImplementation.Loan[] memory loans =
            new MultiVenueArbImplementation.Loan[](1);
        loans[0] = MultiVenueArbImplementation.Loan({
            token: address(loanToken),
            amount: 1 ether,
            provider: MultiVenueArbImplementation.LoanProvider.ERC3156,
            providerAddr: address(feeLender)
        });
        MultiVenueArbImplementation.PlanV2 memory plan = MultiVenueArbImplementation.PlanV2({
            loans: loans,
            cycleSlippageBps: 0,
            steps: new MultiVenueArbImplementation.Step[](0),
            minProfit: 0,
            declaredResidue: 0,
            commitment: bytes32(0),
            chainId: 0,
            deadline: 0
        });

        uint256 repay = 1 ether + (1 ether * feeBps) / 10_000;
        vm.expectRevert(
            abi.encodeWithSelector(DebtNotRepaid.selector, address(loanToken), 1 ether, repay)
        );
        executor.startV2(plan);

        assertEq(loanToken.balanceOf(address(executor)), 0, "nothing was left behind");
    }

    /// The new plan fields are inside the commitment. A field checked *before*
    /// the commitment that is not *in* it could be changed without the
    /// commitment noticing.
    function testTheNewPlanFieldsAreCommitted() external {
        MultiVenueArbImplementation.PlanV2 memory plan = _plan(_donateStep(2 ether), 0);
        bytes32 base = executor.planCommitment(plan);

        plan.chainId = uint64(block.chainid);
        assertTrue(executor.planCommitment(plan) != base, "chainId is not committed");

        plan.chainId = 0;
        plan.deadline = 1;
        assertTrue(executor.planCommitment(plan) != base, "deadline is not committed");
    }
}
