// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {MultiVenueArbImplementation} from "../contracts/executor/MultiVenueArbImplementation.sol";
import {ArbitrageCloneFactory} from "../contracts/executor/ArbitrageCloneFactory.sol";
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

/// Minimal Permit2 stand-in: `initialise` requires a non-zero address and the
/// generic path never touches it.
contract PermitStub {}

/// PLAN.md Task 5.1 Step 1–2: **demonstrate B-1 against the contract as it
/// stands.**
///
/// These tests are written to PASS today. That is the point of them: an
/// asserted vulnerability is a vulnerability somebody has proved, and the
/// alternative — describing it in a document and fixing it in the same commit
/// — leaves no evidence that the hole was ever real.
///
/// Step 3 inverts them into `testNoArbitraryCallSurfaceExists`.
///
/// # What B-1 actually is
///
/// `_execGeneric` decodes `(target, callData, action, token, amount)` from a
/// step's payload and ends with `target.safeCall(callData)` — an arbitrary
/// call, to an arbitrary address, with arbitrary calldata, against no
/// allowlist.
///
/// Before the call it also branches on `action`:
///
/// * `action == 1` grants `target` an ERC-20 allowance over the executor's
///   holdings;
/// * `action == 2` transfers the executor's tokens to `target` outright.
///
/// **Measured, not assumed: those two are not equally dangerous.** The
/// transfer is caught — `InsufficientFinalBalance` fires at settlement and the
/// whole transaction reverts, so the theft is undone. The *allowance* is not:
/// granting one moves no tokens, so the profit invariant sees nothing wrong,
/// the settlement succeeds, and the approval **survives the transaction**. The
/// holder then drains the executor later, from any address, with no executor
/// key involved.
///
/// So the unmitigated hole is narrower than "move the money anywhere" and
/// worse in shape: a single successful settlement can leave behind a standing,
/// unlimited claim on everything the contract will ever hold.
///
/// # Why `onlyExecutor` does not make this acceptable
///
/// Reaching a generic step requires an authorised executor, so this is not
/// open to the public. It is still the highest-severity item in the plan,
/// because **an executor is authorised to trade, not to transfer.** A
/// settlement contract exists so that the key which routes trades cannot also
/// empty the account; `_execGeneric` collapses that distinction, and every
/// operational control above it — signer separation, capital limits, the risk
/// ladder — rests on a boundary the contract does not enforce.
contract NoArbitraryCallTest is Test {
    MultiVenueArbImplementation internal executor;
    MockERC3156Lender internal lender;
    MockERC20 internal loanToken;

    function setUp() public {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        executor = MultiVenueArbImplementation(factory.deployClone(bytes32("b1")));

        loanToken = new MockERC20("Loan", "LN", 18);
        lender = new MockERC3156Lender(address(loanToken), 0);
        PermitStub permit2 = new PermitStub();

        // `address(this)` becomes both owner and the authorised executor, which
        // is what a compromised or careless operator key looks like.
        executor.initialise(address(this), address(1), address(1), address(0), address(permit2), 0, 0, 1);

        loanToken.mint(address(lender), 1_000 ether);
    }

    function _genericStep(bytes memory payload)
        internal
        pure
        returns (MultiVenueArbImplementation.Step[] memory steps)
    {
        steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.GENERIC,
            data: payload
        });
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
            minProfit: 0
        });
    }

    /// B-1, part one: a generic step reaches any target with any calldata.
    function testGenericStepCanCallAnyTarget() external {
        Attacker atk = new Attacker();
        bytes memory payload = abi.encode(
            address(atk),
            abi.encodeCall(Attacker.pwn, ()),
            uint256(0),
            address(0),
            uint256(0)
        );

        // The plan needs enough profit to repay the loan; fund the executor so
        // the settlement invariant is satisfied and the attack is not masked by
        // an unrelated revert.
        loanToken.mint(address(executor), 10 ether);

        executor.startV2(_plan(_genericStep(payload)));

        assertTrue(atk.pwned(), "arbitrary call surface is reachable");
        assertEq(
            atk.lastCaller(),
            address(executor),
            "and the call arrives as the executor, carrying its authority"
        );
    }

    /// B-1, part two — and a **defence that works**, recorded because getting
    /// the severity right matters more than making it sound bad.
    ///
    /// `action == 2` does transfer the executor's tokens to an arbitrary
    /// address. It does not succeed: the settlement's final-balance check sees
    /// the shortfall and reverts the whole transaction, so the transfer is
    /// undone. Measured here rather than reasoned about — the first version of
    /// this test asserted the theft completed, and it failed with
    /// `InsufficientFinalBalance(1.6e19, 2.1e19)`.
    function testGenericStepTransferIsCaughtByTheProfitInvariant() external {
        address thief = address(0xBEEF);
        uint256 stolen = 5 ether;

        loanToken.mint(address(executor), 20 ether);
        uint256 before = loanToken.balanceOf(address(executor));

        bytes memory payload = abi.encode(
            thief,
            bytes(""),
            uint256(2), // action == 2: _safeTransfer(token, target, amount)
            address(loanToken),
            stolen
        );

        vm.expectRevert();
        executor.startV2(_plan(_genericStep(payload)));

        assertEq(loanToken.balanceOf(thief), 0, "the transfer was rolled back");
        assertEq(loanToken.balanceOf(address(executor)), before);
    }

    /// B-1, part three — **the half no existing defence catches.**
    ///
    /// `action == 1` grants an arbitrary spender an allowance over the
    /// executor's holdings. No tokens move, so the final-balance check that
    /// caught the transfer sees nothing wrong and the settlement succeeds. The
    /// approval outlives the transaction.
    function testGenericStepCanGrantAllowanceToAnyAddress() external {
        address spender = address(0xCAFE);
        loanToken.mint(address(executor), 20 ether);

        bytes memory payload = abi.encode(
            spender,
            bytes(""),
            uint256(1), // action == 1: _ensureDirectAllowance(token, target, amount)
            address(loanToken),
            type(uint256).max
        );

        executor.startV2(_plan(_genericStep(payload)));

        assertGt(
            loanToken.allowance(address(executor), spender),
            0,
            "an arbitrary spender holds a standing allowance over the executor"
        );
    }

    /// And the allowance is not theoretical: the holder drains the contract in
    /// a **later transaction**, from an address that was never an executor.
    ///
    /// This is what makes part three the finding rather than part one. An
    /// arbitrary call is bounded by the settlement invariant that runs after
    /// it. A standing approval is not bounded by anything — it is still there
    /// on the next block, and on every block after, against every token
    /// balance the contract acquires.
    function testTheGrantedAllowanceDrainsTheExecutorLater() external {
        address spender = address(0xCAFE);
        loanToken.mint(address(executor), 20 ether);

        bytes memory payload = abi.encode(
            spender,
            bytes(""),
            uint256(1),
            address(loanToken),
            type(uint256).max
        );
        executor.startV2(_plan(_genericStep(payload)));

        // A different transaction, a different sender, no executor authority.
        uint256 balance = loanToken.balanceOf(address(executor));
        assertGt(balance, 0);
        vm.prank(spender);
        loanToken.transferFrom(address(executor), spender, balance);

        assertEq(loanToken.balanceOf(address(executor)), 0, "the executor was emptied");
        assertEq(loanToken.balanceOf(spender), balance);
    }

    /// The boundary that does hold, stated so the severity is not overclaimed:
    /// an unauthorised caller cannot reach any of this.
    function testAnUnauthorisedCallerCannotReachTheGenericStep() external {
        Attacker atk = new Attacker();
        bytes memory payload =
            abi.encode(address(atk), abi.encodeCall(Attacker.pwn, ()), uint256(0), address(0), uint256(0));

        vm.prank(address(0xDEAD));
        vm.expectRevert();
        executor.startV2(_plan(_genericStep(payload)));

        assertFalse(atk.pwned(), "onlyExecutor holds; the surface is not public");
    }
}
