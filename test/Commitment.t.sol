// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {MultiVenueArbImplementation} from "../contracts/executor/MultiVenueArbImplementation.sol";
import {ArbitrageCloneFactory} from "../contracts/executor/ArbitrageCloneFactory.sol";
import {MockERC20} from "../contracts/mocks/MockERC20.sol";
import {MockERC3156Lender} from "../contracts/mocks/MockERC3156Lender.sol";
import {Test} from "forge-std/Test.sol";

contract PermitStub {}

contract Donor {
    function donate(address token, address to, uint256 amount) external {
        MockERC20(token).transfer(to, amount);
    }
}

/// Task 5.3 — §25 / INV-06.
///
/// The commitment binds a plan to the trade it was simulated, risk-checked and
/// sized for. The on-chain half catches drift between the planner and the
/// chain: an encoder that builds a plan the planner did not describe, a field
/// dropped by an ABI change, a transport that corrupts a word. It does not
/// constrain a malicious executor — plan and commitment arrive in the same
/// calldata — and the tests say so rather than implying otherwise.
contract CommitmentTest is Test {
    uint16 internal constant DONOR_ID = 1;

    MultiVenueArbImplementation internal executor;
    MockERC3156Lender internal lender;
    MockERC20 internal loanToken;
    Donor internal donor;

    function setUp() public {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        executor = MultiVenueArbImplementation(factory.deployClone(bytes32("commit")));

        loanToken = new MockERC20("Loan", "LN", 18);
        lender = new MockERC3156Lender(address(loanToken), 0);
        donor = new Donor();
        executor.initialise(address(this), address(1), address(1), address(0), address(new PermitStub()), 0, 0, 1);

        loanToken.mint(address(lender), 1_000 ether);
        loanToken.mint(address(donor), 100 ether);
        executor.registerAdapter(DONOR_ID, address(donor));
        executor.allowSelector(DONOR_ID, Donor.donate.selector);
    }

    function _plan(uint256 profit)
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

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.GENERIC,
            data: abi.encode(
                DONOR_ID,
                address(0),
                uint256(0),
                abi.encodeCall(Donor.donate, (address(loanToken), address(executor), profit))
            )
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

    function _committed(uint256 profit)
        internal
        view
        returns (MultiVenueArbImplementation.PlanV2 memory plan)
    {
        plan = _plan(profit);
        plan.commitment = executor.planCommitment(plan);
    }

    /// A committed plan executes.
    function testACommittedPlanExecutes() external {
        assertEq(executor.startV2(_committed(2 ether)), 2 ether);
    }

    /// Mutating any committed field after computing the commitment reverts,
    /// and the revert carries both hashes so the mismatch can be diffed
    /// against the ticket rather than merely observed.
    function testCommitmentMismatchReverts() external {
        MultiVenueArbImplementation.PlanV2 memory plan = _committed(2 ether);
        bytes32 declared = plan.commitment;

        plan.minProfit = 1; // one field, changed after the fact

        bytes32 recomputed = executor.planCommitment(plan);
        assertTrue(recomputed != declared);
        vm.expectRevert(
            abi.encodeWithSelector(
                MultiVenueArbImplementation.CommitmentMismatch.selector, declared, recomputed
            )
        );
        executor.startV2(plan);
    }

    /// Every committed field, one at a time. A field that can be changed
    /// without moving the commitment is a field the commitment does not
    /// protect, and finding that out later is the whole failure mode.
    function testEveryCommittedFieldMovesTheCommitment() external {
        MultiVenueArbImplementation.PlanV2 memory base = _plan(2 ether);
        bytes32 h = executor.planCommitment(base);

        MultiVenueArbImplementation.PlanV2 memory p = _plan(2 ether);
        p.minProfit = 1;
        assertTrue(executor.planCommitment(p) != h, "minProfit");

        p = _plan(2 ether);
        p.cycleSlippageBps = 7;
        assertTrue(executor.planCommitment(p) != h, "cycleSlippageBps");

        p = _plan(2 ether);
        p.declaredResidue = 1;
        assertTrue(executor.planCommitment(p) != h, "declaredResidue");

        p = _plan(2 ether);
        p.loans[0].amount += 1;
        assertTrue(executor.planCommitment(p) != h, "loan amount");

        p = _plan(2 ether);
        p.loans[0].token = address(0xBEEF);
        assertTrue(executor.planCommitment(p) != h, "loan token");

        p = _plan(2 ether);
        p.loans[0].providerAddr = address(0xBEEF);
        assertTrue(executor.planCommitment(p) != h, "provider address");

        // The step payload -- where the adapter id and the calldata live.
        p = _plan(3 ether);
        assertTrue(executor.planCommitment(p) != h, "step data");

        p = _plan(2 ether);
        p.steps[0].op = MultiVenueArbImplementation.Op.UNIV3;
        assertTrue(executor.planCommitment(p) != h, "step op");
    }

    /// Two steps whose payloads concatenate to the same bytes must not hash
    /// the same. Hashing each step's data rather than concatenating is what
    /// stops a long payload being split across a boundary to collide with a
    /// different step list.
    function testStepBoundariesCannotBeCollided() external {
        MultiVenueArbImplementation.PlanV2 memory a = _plan(2 ether);
        a.steps = new MultiVenueArbImplementation.Step[](2);
        a.steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.GENERIC,
            data: hex"aabb"
        });
        a.steps[1] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.GENERIC,
            data: hex"cc"
        });

        MultiVenueArbImplementation.PlanV2 memory b = _plan(2 ether);
        b.steps = new MultiVenueArbImplementation.Step[](2);
        b.steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.GENERIC,
            data: hex"aa"
        });
        b.steps[1] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.GENERIC,
            data: hex"bbcc"
        });

        assertTrue(
            executor.planCommitment(a) != executor.planCommitment(b),
            "a split payload collided with a different step list"
        );
    }

    /// The commitment binds the plan to THIS deployment. A plan committed for
    /// one executor must not execute on another — INV-05's wrong-chain
    /// submission, expressed where it can be enforced.
    function testACommitmentDoesNotTransferToAnotherExecutor() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        MultiVenueArbImplementation other =
            MultiVenueArbImplementation(factory.deployClone(bytes32("other")));
        other.initialise(address(this), address(1), address(1), address(0), address(new PermitStub()), 0, 0, 1);

        MultiVenueArbImplementation.PlanV2 memory plan = _committed(2 ether);
        assertTrue(
            other.planCommitment(plan) != plan.commitment,
            "the same plan hashes differently on a different executor"
        );
    }

    /// ...and to this chain.
    function testACommitmentDoesNotTransferToAnotherChain() external {
        MultiVenueArbImplementation.PlanV2 memory plan = _plan(2 ether);
        bytes32 onBase = executor.planCommitment(plan);
        vm.chainId(1);
        assertTrue(executor.planCommitment(plan) != onBase, "chain id is not bound");
    }

    /// An uncommitted plan still runs. The encoder that fills this field
    /// arrives with Phase 7; until then a zero commitment is "not committed",
    /// not "committed to zero", and Phase 7 is where it becomes a rejection.
    function testAnUncommittedPlanIsStillAccepted() external {
        assertEq(executor.startV2(_plan(2 ether)), 2 ether);
    }

    /// The limit of the on-chain half, asserted rather than left implicit: a
    /// caller who changes the plan can change the commitment with it. What the
    /// check catches is drift between the planner and the chain, not an
    /// executor that means to run something else.
    function testTheOnChainCheckDoesNotConstrainTheCallerItself() external {
        MultiVenueArbImplementation.PlanV2 memory plan = _plan(3 ether);
        plan.commitment = executor.planCommitment(plan); // recommitted to the change
        assertEq(
            executor.startV2(plan),
            3 ether,
            "a caller that recommits executes -- the off-chain refusal to sign is what stops that"
        );
    }
}
