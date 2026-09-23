// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {MultiVenueArbImplementation} from "../contracts/executor/MultiVenueArbImplementation.sol";
import {ArbitrageCloneFactory} from "../contracts/executor/ArbitrageCloneFactory.sol";
import {
    UnknownAdapter,
    InvalidAdapter,
    AdapterAlreadyRegistered
} from "../contracts/core/AdapterRegistry.sol";
import {NotOwner} from "../contracts/executor/MultiVenueArbImplementation.sol";
import {MockERC20} from "../contracts/mocks/MockERC20.sol";
import {MockERC3156Lender} from "../contracts/mocks/MockERC3156Lender.sol";
import {Test} from "forge-std/Test.sol";

/// A contract with no business being called by a settlement executor.
contract Attacker {
    bool public pwned;
    address public lastCaller;

    function pwn() external {
        pwned = true;
        lastCaller = msg.sender;
    }
}

/// A registered adapter, standing in for a venue.
contract Donor {
    function donate(address token, address to, uint256 amount) external {
        MockERC20(token).transfer(to, amount);
    }
}

contract PermitStub {}

/// B-1 closed. These tests were written to **pass against the vulnerable
/// contract** and are now inverted — the git history holds both halves, which
/// is the evidence that the hole was real and is gone.
///
/// What was proved before the fix, and what the fix does about each:
///
/// | Before | Now |
/// |---|---|
/// | a step reached any target with any calldata | the target is resolved from the registry; the payload has no target field to supply |
/// | a step granted any address an allowance over the executor | the approval spender **is** the resolved adapter; there is no expression a payload can steer |
/// | a step transferred tokens to any address (caught by the balance check, but a second way to reach a target) | the transfer path is gone entirely |
contract NoArbitraryCallTest is Test {
    uint16 internal constant DONOR_ID = 1;

    MultiVenueArbImplementation internal executor;
    MockERC3156Lender internal lender;
    MockERC20 internal loanToken;
    Donor internal donor;

    function setUp() public {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        executor = MultiVenueArbImplementation(factory.deployClone(bytes32("b1")));

        loanToken = new MockERC20("Loan", "LN", 18);
        lender = new MockERC3156Lender(address(loanToken), 0);
        donor = new Donor();
        PermitStub permit2 = new PermitStub();

        executor.initialise(address(this), address(1), address(1), address(0), address(permit2), 0, 0, 1);
        loanToken.mint(address(lender), 1_000 ether);
    }

    function _step(bytes memory payload)
        internal
        pure
        returns (MultiVenueArbImplementation.Step[] memory steps)
    {
        steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] =
            MultiVenueArbImplementation.Step({op: MultiVenueArbImplementation.Op.GENERIC, data: payload});
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

    /// The inversion of `testGenericStepCanCallAnyTarget`.
    ///
    /// An unregistered id is refused by name, so the failure says *why* rather
    /// than surfacing as an undifferentiated revert.
    function testNoArbitraryCallSurfaceExists() external {
        Attacker atk = new Attacker();
        loanToken.mint(address(executor), 10 ether);

        // The attacker's address cannot appear in a payload at all; the only
        // way to aim a step is by id, and this one is not registered.
        uint16 unregistered = 99;
        bytes memory payload =
            abi.encode(unregistered, address(0), uint256(0), abi.encodeCall(Attacker.pwn, ()));

        vm.expectRevert(abi.encodeWithSelector(UnknownAdapter.selector, unregistered));
        executor.startV2(_plan(_step(payload)));

        assertFalse(atk.pwned(), "the arbitrary call surface is gone");
    }

    /// Registering the attacker is the only route left, and it is `onlyOwner`
    /// — which is the separation the whole fix rests on: an executor may
    /// trade, and may not choose what the contract calls.
    function testAnExecutorCannotRegisterItsOwnTarget() external {
        Attacker atk = new Attacker();
        address executorKey = address(0xE0);
        executor.setExecutor(executorKey, true);

        vm.prank(executorKey);
        vm.expectRevert(NotOwner.selector);
        executor.registerAdapter(42, address(atk));

        assertEq(executor.adapterOf(42), address(0));
    }

    /// The old payload shape is not merely rejected, it no longer decodes.
    /// `(address, bytes, uint256, address, uint256)` is not
    /// `(uint16, address, uint256, bytes)`.
    function testTheOldGenericPayloadNoLongerDecodes() external {
        Attacker atk = new Attacker();
        loanToken.mint(address(executor), 10 ether);

        bytes memory legacyPayload = abi.encode(
            address(atk), abi.encodeCall(Attacker.pwn, ()), uint256(0), address(0), uint256(0)
        );

        vm.expectRevert();
        executor.startV2(_plan(_step(legacyPayload)));
        assertFalse(atk.pwned());
    }

    /// The approval spender is the resolved adapter. A payload can ask for an
    /// approval; it cannot say who receives it.
    function testAnApprovalCanOnlyEverNameTheResolvedAdapter() external {
        executor.registerAdapter(DONOR_ID, address(donor));
        executor.allowSelector(DONOR_ID, Donor.donate.selector);
        loanToken.mint(address(executor), 20 ether);

        address outsider = address(0xCAFE);
        bytes memory payload = abi.encode(
            DONOR_ID,
            address(loanToken),
            type(uint256).max,
            abi.encodeCall(Donor.donate, (address(loanToken), address(executor), 0))
        );
        executor.startV2(_plan(_step(payload)));

        assertGt(
            loanToken.allowance(address(executor), address(donor)),
            0,
            "the registered adapter may be approved -- a router has to pull"
        );
        assertEq(
            loanToken.allowance(address(executor), outsider),
            0,
            "and nobody else can be, whatever the payload says"
        );
    }

    /// The substitution preserved the mechanism: a registered adapter still
    /// settles a profitable plan. A fix that closed the hole by breaking
    /// settlement would not be a fix.
    function testARegisteredAdapterStillSettlesAPlan() external {
        executor.registerAdapter(DONOR_ID, address(donor));
        executor.allowSelector(DONOR_ID, Donor.donate.selector);
        loanToken.mint(address(donor), 5 ether);

        bytes memory payload = abi.encode(
            DONOR_ID,
            address(0),
            uint256(0),
            abi.encodeCall(Donor.donate, (address(loanToken), address(executor), 2 ether))
        );
        uint256 profit = executor.startV2(_plan(_step(payload)));
        assertEq(profit, 2 ether, "the adapter path still realises profit");
    }

    /// An id may not be silently re-pointed. Replacing an adapter is two
    /// deliberate transactions, so a substitution cannot ride in on one.
    function testAnAdapterIdCannotBeSilentlyRepointed() external {
        Attacker atk = new Attacker();
        executor.registerAdapter(DONOR_ID, address(donor));
        executor.allowSelector(DONOR_ID, Donor.donate.selector);

        vm.expectRevert(abi.encodeWithSelector(AdapterAlreadyRegistered.selector, DONOR_ID));
        executor.registerAdapter(DONOR_ID, address(atk));
        assertEq(executor.adapterOf(DONOR_ID), address(donor));

        // The deliberate path still works, and leaves a trace.
        executor.deregisterAdapter(DONOR_ID);
        executor.registerAdapter(DONOR_ID, address(atk));
        assertEq(executor.adapterOf(DONOR_ID), address(atk));
    }

    /// An address with no code is never registered. A step naming it would
    /// `safeCall` into nothing and **succeed silently**, which is the failure
    /// an allowlist exists to prevent.
    function testAnAddressWithNoCodeIsNeverRegistered() external {
        vm.expectRevert(InvalidAdapter.selector);
        executor.registerAdapter(7, address(0xDEAD));

        vm.expectRevert(InvalidAdapter.selector);
        executor.registerAdapter(7, address(0));
    }

    /// The boundary that always held, kept so the severity is not overstated.
    function testAnUnauthorisedCallerStillCannotStart() external {
        executor.registerAdapter(DONOR_ID, address(donor));
        executor.allowSelector(DONOR_ID, Donor.donate.selector);
        bytes memory payload = abi.encode(DONOR_ID, address(0), uint256(0), bytes(""));

        vm.prank(address(0xDEAD));
        vm.expectRevert();
        executor.startV2(_plan(_step(payload)));
    }
}
