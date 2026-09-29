// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Test, console} from "forge-std/Test.sol";
import {MultiVenueArbImplementation} from "../contracts/executor/MultiVenueArbImplementation.sol";

/// The cross-language differential for INV-06's commitment.
///
/// `apex-exec` recomputes `planCommitment` in Rust, because the signer has to
/// produce the value this contract will recompute and revert on. A Rust mirror
/// of a Solidity hash drifts unless something holds the two together, and the
/// honest thing to hold them together with is **the contract's own answer**.
///
/// So this test computes the commitment for a fixed set of plans and writes them
/// to `crates/apex-exec/tests/fixtures/plan_commitments.json`, which is tracked.
/// `apex-exec`'s `plan_commitment_matches_the_contract` asserts the Rust encoder
/// reproduces every one. A change to either half turns one of them red:
///
/// - Change the contract's hash and this test rewrites the fixture, which the
///   Rust test then rejects.
/// - Change the Rust encoder and the Rust test rejects it against the fixture
///   this test already wrote.
///
/// The cases are chosen to move each field independently, and to include the
/// shapes where a naive encoder goes wrong: an empty loan list and an empty step
/// list (whose rolling hashes must stay `bytes32(0)`), two steps whose payloads
/// concatenate to one longer payload, and two loans in both orders.
contract PlanCommitmentFixtureTest is Test {
    MultiVenueArbImplementation internal executor;

    address internal constant TOKEN_A = 0x1111111111111111111111111111111111111111;
    address internal constant TOKEN_B = 0x2222222222222222222222222222222222222222;
    address internal constant LENDER = 0x3333333333333333333333333333333333333333;

    function setUp() external {
        executor = new MultiVenueArbImplementation();
    }

    function _empty() internal pure returns (MultiVenueArbImplementation.PlanV2 memory p) {
        p.loans = new MultiVenueArbImplementation.Loan[](0);
        p.steps = new MultiVenueArbImplementation.Step[](0);
        p.cycleSlippageBps = 0;
        p.minProfit = 0;
        p.declaredResidue = 0;
        p.commitment = bytes32(0);
        p.chainId = 0;
        p.deadline = 0;
    }

    function _oneLoan(uint256 amount)
        internal
        pure
        returns (MultiVenueArbImplementation.Loan[] memory loans)
    {
        loans = new MultiVenueArbImplementation.Loan[](1);
        loans[0] = MultiVenueArbImplementation.Loan({
            token: TOKEN_A,
            amount: amount,
            provider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            providerAddr: LENDER
        });
    }

    function _steps(bytes memory a, bytes memory b)
        internal
        pure
        returns (MultiVenueArbImplementation.Step[] memory steps)
    {
        steps = new MultiVenueArbImplementation.Step[](2);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.UNIV3,
            data: a
        });
        steps[1] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.BALANCER,
            data: b
        });
    }

    /// Writes the fixture. Not named `test...` on purpose — it is a generator,
    /// and `testPlanCommitmentFixtureIsCurrent` is the assertion.
    function _cases()
        internal
        view
        returns (string[] memory names, bytes32[] memory hashes)
    {
        names = new string[](6);
        hashes = new bytes32[](6);
        uint256 i;

        MultiVenueArbImplementation.PlanV2 memory p = _empty();
        names[i] = "empty";
        hashes[i++] = executor.planCommitment(p);

        p = _empty();
        p.loans = _oneLoan(1_000_000_000_000_000_000);
        names[i] = "one_loan";
        hashes[i++] = executor.planCommitment(p);

        p = _empty();
        p.loans = _oneLoan(1_000_000_000_000_000_000);
        p.steps = _steps(hex"aabb", hex"ccdd");
        p.cycleSlippageBps = 30;
        names[i] = "loan_and_steps";
        hashes[i++] = executor.planCommitment(p);

        // The boundary case the contract's own comment names: two payloads that
        // concatenate to what a single longer payload would be.
        p = _empty();
        p.steps = _steps(hex"aabbcc", hex"dd");
        names[i] = "split_payload_a";
        hashes[i++] = executor.planCommitment(p);

        p = _empty();
        p.steps = _steps(hex"aa", hex"bbccdd");
        names[i] = "split_payload_b";
        hashes[i++] = executor.planCommitment(p);

        p = _empty();
        p.loans = _oneLoan(1);
        p.steps = _steps(hex"aabb", hex"ccdd");
        p.cycleSlippageBps = 65_535;
        p.minProfit = type(uint256).max;
        p.declaredResidue = 7;
        p.chainId = 8453;
        p.deadline = 1_781_049_614;
        names[i] = "every_field_set";
        hashes[i++] = executor.planCommitment(p);
    }

    /// **The assertion.** The tracked fixture still describes this contract.
    ///
    /// Read-only on purpose. A test that *wrote* the fixture would rewrite it on
    /// every `forge test`, leave CI with a dirty tree, and — worse — make the
    /// two sides agree by construction: the Solidity half would regenerate
    /// whatever the contract now says and the Rust half would be asserting
    /// against a moving target. Reading it means a change to the contract's hash
    /// fails **here**, loudly, with the case name that moved.
    ///
    /// Regenerating is therefore a deliberate act: run
    /// `forge test --match-test testPlanCommitmentFixtureIsCurrent -vv` to see
    /// the new values, and update the file with the reason in the commit.
    function testPlanCommitmentFixtureIsCurrent() external {
        string memory json = vm.readFile("./crates/apex-exec/tests/fixtures/plan_commitments.json");
        (string[] memory names, bytes32[] memory hashes) = _cases();

        assertEq(
            vm.parseJsonUint(json, ".block_chain_id"),
            block.chainid,
            "the fixture was produced on another chain id"
        );

        for (uint256 i; i < names.length; ++i) {
            bytes32 recorded =
                vm.parseJsonBytes32(json, string.concat(".cases.", names[i]));
            if (recorded != hashes[i]) {
                emit log_named_string("case", names[i]);
                emit log_named_bytes32("recorded", recorded);
                emit log_named_bytes32("contract", hashes[i]);
            }
            assertEq(recorded, hashes[i], names[i]);
        }
    }

    /// The property the step hash exists for: two plans differing only in where a
    /// payload boundary falls must not collide.
    function testAPayloadBoundaryMovesTheCommitment() external {
        (, bytes32[] memory hashes) = _cases();
        assertTrue(hashes[3] != hashes[4], "a payload boundary moved without moving the hash");
    }

    /// Every hash in the set is distinct, so the fixture is exercising six
    /// different preimages rather than six names for one.
    function testEveryFixtureCaseIsDistinct() external {
        (string[] memory names, bytes32[] memory hashes) = _cases();
        for (uint256 i; i < hashes.length; ++i) {
            for (uint256 j = i + 1; j < hashes.length; ++j) {
                assertTrue(
                    hashes[i] != hashes[j],
                    string.concat(names[i], " collides with ", names[j])
                );
            }
        }
    }

    /// **The other half of the encoder differential.** The bytes `apex-exec`
    /// produces decode, here, into the fields they claim.
    ///
    /// Decoding rather than hashing on purpose: an ABI offset that is wrong by
    /// one word does not revert, it decodes as garbage. A hash comparison would
    /// say the bytes changed; this says what they now mean.
    ///
    /// Neither side generates `univ3_steps.json` — `apex-exec` asserts it
    /// *encodes* to those bytes and this asserts they *decode* to those fields.
    function testUniV3StepsDecode() external view {
        string memory json = vm.readFile("./crates/apex-exec/tests/fixtures/univ3_steps.json");
        string[3] memory names = ["one_hop_500", "two_hop_500_3000", "min_out_one"];

        for (uint256 i; i < names.length; ++i) {
            string memory base = string.concat(".cases.", names[i]);
            bytes memory encoded = vm.parseJsonBytes(json, string.concat(base, ".encoded"));
            bytes memory expectedPath = vm.parseJsonBytes(json, string.concat(base, ".path"));
            uint256 expectedIn = vm.parseJsonUint(json, string.concat(base, ".amount_in"));
            uint256 expectedMin = vm.parseJsonUint(json, string.concat(base, ".min_out"));

            (bytes memory path, uint256 amountIn, uint256 minOut) =
                abi.decode(encoded, (bytes, uint256, uint256));

            assertEq(path, expectedPath, string.concat(names[i], ": path"));
            assertEq(amountIn, expectedIn, string.concat(names[i], ": amountIn"));
            assertEq(minOut, expectedMin, string.concat(names[i], ": minOut"));
            assertTrue(minOut > 0, "a zero minimum accepts any output");
        }
    }

    /// Prints the current values, for the deliberate regeneration above.
    function testPrintPlanCommitments() external view {
        (string[] memory names, bytes32[] memory hashes) = _cases();
        console.log("block_chain_id", block.chainid);
        console.log("executor", address(executor));
        for (uint256 i; i < names.length; ++i) {
            console.log(names[i], vm.toString(hashes[i]));
        }
    }
}
