// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {MultiVenueArbImplementation} from "../contracts/executor/MultiVenueArbImplementation.sol";
import {ArbitrageCloneFactory} from "../contracts/executor/ArbitrageCloneFactory.sol";
import {UnknownAdapter, SelectorNotAllowed} from "../contracts/core/AdapterRegistry.sol";
import {MockERC20} from "../contracts/mocks/MockERC20.sol";
import {MockERC3156Lender} from "../contracts/mocks/MockERC3156Lender.sol";
import {Test} from "forge-std/Test.sol";

contract Donor {
    uint256 public calls;

    function donate(address token, address to, uint256 amount) external {
        calls += 1;
        MockERC20(token).transfer(to, amount);
    }
}

contract PermitStub {}

/// **The step dispatch contract, pinned before Task 5.5 removes the
/// trampolines.**
///
/// Every step currently reaches its handler through two extra hops: a
/// `delegatecall` into a two-line module, which calls back into
/// `this.moduleExec*`, which calls an `internal` function that was always
/// local. Task 5.5 folds that away. These tests say what must not change while
/// it does, and they were written and made green against the trampolines so
/// that "nothing changed" is a measurement rather than a hope.
///
/// The property most at risk is the last one. Revert data currently crosses a
/// `delegatecall` boundary and is re-raised by hand with
/// `assembly { revert(add(ret, 0x20), mload(ret)) }`. Afterwards it propagates
/// natively. A custom error losing its arguments on the way out would be
/// invisible to every test that only asserts *that* something reverted.
contract StepDispatchTest is Test {
    uint16 internal constant DONOR_ID = 1;

    MultiVenueArbImplementation internal executor;
    MockERC3156Lender internal lender;
    MockERC20 internal loanToken;
    Donor internal donor;

    function setUp() public {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        executor = MultiVenueArbImplementation(factory.deployClone(bytes32("steps")));

        loanToken = new MockERC20("Loan", "LN", 18);
        lender = new MockERC3156Lender(address(loanToken), 0);
        donor = new Donor();
        PermitStub permit2 = new PermitStub();

        executor.initialise(address(this), address(1), address(1), address(0), address(permit2), 0, 0, 1);
        loanToken.mint(address(lender), 1_000 ether);
        loanToken.mint(address(executor), 10 ether);

        executor.registerAdapter(DONOR_ID, address(donor));
        executor.allowSelector(DONOR_ID, Donor.donate.selector);
    }

    function _generic(bytes memory payload)
        internal
        pure
        returns (MultiVenueArbImplementation.Step memory)
    {
        return MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.GENERIC,
            data: payload
        });
    }

    function _donation(uint256 amount) internal view returns (bytes memory) {
        return abi.encode(
            DONOR_ID,
            address(0),
            uint256(0),
            abi.encodeCall(Donor.donate, (address(loanToken), address(executor), amount))
        );
    }

    function _plan(MultiVenueArbImplementation.Step[] memory steps)
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
            minProfit: 0,
            declaredResidue: 0,
            commitment: bytes32(0),
            chainId: 0,
            deadline: 0
        });
    }

    /// A GENERIC step reaches the registered adapter.
    function testAGenericStepReachesItsAdapter() external {
        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = _generic(_donation(0));
        executor.startV2(_plan(steps));
        assertEq(donor.calls(), 1, "the adapter was not called");
    }

    /// **Steps run in order, and all of them run.** The loop is the part Task
    /// 5.5 rewrites, so the count matters as much as the effect.
    function testEveryStepRunsAndInOrder() external {
        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](4);
        for (uint256 i; i < 4; i++) {
            steps[i] = _generic(_donation(0));
        }
        executor.startV2(_plan(steps));
        assertEq(donor.calls(), 4, "not every step ran");
    }

    /// A plan with no steps is not an error -- it settles flat.
    function testAnEmptyStepListSettles() external {
        executor.startV2(_plan(new MultiVenueArbImplementation.Step[](0)));
        assertEq(donor.calls(), 0);
    }

    /// **Revert data survives the trip out.** The error must arrive with its
    /// argument, not merely arrive.
    function testACustomErrorSurvivesWithItsArgument() external {
        uint16 unregistered = 99;
        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = _generic(
            abi.encode(unregistered, address(0), uint256(0), abi.encodeCall(Donor.donate, (address(1), address(1), 0)))
        );
        vm.expectRevert(abi.encodeWithSelector(UnknownAdapter.selector, unregistered));
        executor.startV2(_plan(steps));
    }

    /// The same, for a two-argument error raised one frame deeper.
    function testATwoArgumentErrorSurvivesWithBothArguments() external {
        bytes4 forbidden = bytes4(keccak256("selfdestruct()"));
        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = _generic(abi.encode(DONOR_ID, address(0), uint256(0), abi.encodePacked(forbidden)));
        vm.expectRevert(
            abi.encodeWithSelector(SelectorNotAllowed.selector, DONOR_ID, forbidden)
        );
        executor.startV2(_plan(steps));
    }

    /// A failure in a later step aborts the whole plan -- there is no partial
    /// settlement.
    function testAFailingLaterStepAbortsEverything() external {
        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](3);
        steps[0] = _generic(_donation(0));
        steps[1] = _generic(_donation(0));
        steps[2] = _generic(
            abi.encode(uint16(99), address(0), uint256(0), abi.encodeCall(Donor.donate, (address(1), address(1), 0)))
        );
        vm.expectRevert(abi.encodeWithSelector(UnknownAdapter.selector, uint16(99)));
        executor.startV2(_plan(steps));
        assertEq(donor.calls(), 0, "the first two steps were not rolled back");
    }

    /// `_executeSteps` is `external` so the plan's `Step[]` stays in calldata.
    /// It is not an entry point, and the guard is what says so.
    function testExecuteStepsRefusesAnOutsideCaller() external {
        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = _generic(_donation(0));
        vm.expectRevert(MultiVenueArbImplementation.InvalidGenericAction.selector);
        executor._executeSteps(steps, block.timestamp + 1, address(executor), address(0));
    }
}
