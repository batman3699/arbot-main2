// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Test, console} from "forge-std/Test.sol";
import {MultiVenueArbImplementation} from "../contracts/executor/MultiVenueArbImplementation.sol";

/// The cross-language differential for `startV2`'s calldata.
///
/// `apex-exec` ABI-encodes `startV2(PlanV2)` in Rust, because what the signer
/// signs is those bytes. `PlanV2` is a nested dynamic tuple — two dynamic arrays,
/// one of which holds dynamic elements — which is exactly where a hand-written
/// encoder goes wrong by one word, and a word-off offset does not revert: it
/// decodes as a different plan.
///
/// So `crates/apex-exec/tests/fixtures/start_v2_calldata.json` holds the bytes
/// **Solidity's own `abi.encodeCall` produces** for a fixed set of plans:
///
/// - `testStartV2CalldataFixtureIsCurrent` asserts the tracked bytes still equal
///   the compiler's encoding, so a change to `PlanV2` fails here by case name.
/// - `testStartV2CalldataDecodes` decodes the tracked bytes and asserts every
///   field, so the fixture means what its case says.
/// - `apex-exec`'s `start_v2_calldata_matches_the_compiler` asserts the Rust
///   encoder reproduces every byte.
///
/// **Neither side writes the file.** Regenerate deliberately with
/// `forge test --match-test testPrintStartV2Calldata -vv` and say why in the
/// commit.
///
/// The cases move every field, and include the shapes where a naive encoder goes
/// wrong: empty arrays (length word only, no elements), payloads of 2, 3, 32 and
/// 33 bytes (no padding, partial padding, an exact word, one byte past a word),
/// an empty payload, and every numeric field at its type's maximum.
contract StartV2CalldataTest is Test {
    address internal constant TOKEN_A = 0x1111111111111111111111111111111111111111;
    address internal constant TOKEN_B = 0x2222222222222222222222222222222222222222;
    address internal constant LENDER_A = 0x3333333333333333333333333333333333333333;
    address internal constant LENDER_B = 0x4444444444444444444444444444444444444444;

    function _names() internal pure returns (string[3] memory) {
        return ["empty_lists", "one_loan_two_steps", "two_loans_three_steps"];
    }

    function _plan(uint256 i) internal pure returns (MultiVenueArbImplementation.PlanV2 memory p) {
        if (i == 0) {
            p.loans = new MultiVenueArbImplementation.Loan[](0);
            p.steps = new MultiVenueArbImplementation.Step[](0);
            p.cycleSlippageBps = 0;
            p.minProfit = 1;
            p.declaredResidue = 0;
            p.commitment = bytes32(uint256(0xc0ffee));
            p.chainId = 8453;
            p.deadline = 1_781_049_614;
        } else if (i == 1) {
            p.loans = new MultiVenueArbImplementation.Loan[](1);
            p.loans[0] = MultiVenueArbImplementation.Loan({
                token: TOKEN_A,
                amount: 1_000_000_000_000_000_000,
                provider: MultiVenueArbImplementation.LoanProvider.BALANCER,
                providerAddr: LENDER_A
            });
            p.steps = new MultiVenueArbImplementation.Step[](2);
            p.steps[0] = MultiVenueArbImplementation.Step({
                op: MultiVenueArbImplementation.Op.UNIV3,
                data: hex"aabb"
            });
            p.steps[1] = MultiVenueArbImplementation.Step({
                op: MultiVenueArbImplementation.Op.GENERIC,
                data: hex"ccddee"
            });
            p.cycleSlippageBps = 30;
            p.minProfit = 1_234_567;
            p.declaredResidue = 0;
            p.commitment = bytes32(0xabababababababababababababababababababababababababababababababab);
            p.chainId = 8453;
            p.deadline = 1_781_049_614;
        } else {
            p.loans = new MultiVenueArbImplementation.Loan[](2);
            p.loans[0] = MultiVenueArbImplementation.Loan({
                token: TOKEN_A,
                amount: 5,
                provider: MultiVenueArbImplementation.LoanProvider.AAVE,
                providerAddr: LENDER_A
            });
            p.loans[1] = MultiVenueArbImplementation.Loan({
                token: TOKEN_B,
                amount: type(uint256).max,
                provider: MultiVenueArbImplementation.LoanProvider.UNIV3,
                providerAddr: LENDER_B
            });
            p.steps = new MultiVenueArbImplementation.Step[](3);
            // 33 bytes: one byte past a word, so the padding is 31 bytes.
            p.steps[0] = MultiVenueArbImplementation.Step({
                op: MultiVenueArbImplementation.Op.BALANCER,
                data: hex"5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a"
            });
            // Exactly one word: no padding at all.
            p.steps[1] = MultiVenueArbImplementation.Step({
                op: MultiVenueArbImplementation.Op.UNIV3,
                data: hex"6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b"
            });
            // Empty: a length word of zero and nothing after it.
            p.steps[2] = MultiVenueArbImplementation.Step({
                op: MultiVenueArbImplementation.Op.GENERIC,
                data: hex""
            });
            p.cycleSlippageBps = type(uint16).max;
            p.minProfit = type(uint256).max;
            p.declaredResidue = 7;
            p.commitment = bytes32(0xfefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefe);
            p.chainId = type(uint64).max;
            p.deadline = type(uint64).max;
        }
    }

    function _encoded(uint256 i) internal pure returns (bytes memory) {
        return abi.encodeCall(MultiVenueArbImplementation.startV2, (_plan(i)));
    }

    /// **The assertion.** The tracked bytes are still the compiler's encoding.
    function testStartV2CalldataFixtureIsCurrent() external view {
        string memory json = vm.readFile("./crates/apex-exec/tests/fixtures/start_v2_calldata.json");
        assertEq(
            bytes4(vm.parseJsonBytes(json, ".selector")),
            MultiVenueArbImplementation.startV2.selector,
            "selector"
        );
        string[3] memory names = _names();
        for (uint256 i; i < names.length; ++i) {
            bytes memory recorded = vm.parseJsonBytes(json, string.concat(".cases.", names[i]));
            assertEq(recorded, _encoded(i), names[i]);
        }
    }

    /// **The other half.** The tracked bytes decode, here, into the plan their
    /// case describes — field by field, rather than by comparing a hash, because
    /// a hash would say the bytes changed and this says what they now mean.
    function testStartV2CalldataDecodes() external view {
        string memory json = vm.readFile("./crates/apex-exec/tests/fixtures/start_v2_calldata.json");
        string[3] memory names = _names();
        for (uint256 i; i < names.length; ++i) {
            bytes memory recorded = vm.parseJsonBytes(json, string.concat(".cases.", names[i]));
            MultiVenueArbImplementation.PlanV2 memory got = this.decodeStartV2(recorded);
            MultiVenueArbImplementation.PlanV2 memory want = _plan(i);

            assertEq(got.loans.length, want.loans.length, string.concat(names[i], ": loans"));
            for (uint256 j; j < want.loans.length; ++j) {
                assertEq(got.loans[j].token, want.loans[j].token, "loan token");
                assertEq(got.loans[j].amount, want.loans[j].amount, "loan amount");
                assertEq(uint8(got.loans[j].provider), uint8(want.loans[j].provider), "loan provider");
                assertEq(got.loans[j].providerAddr, want.loans[j].providerAddr, "loan providerAddr");
            }
            assertEq(got.steps.length, want.steps.length, string.concat(names[i], ": steps"));
            for (uint256 j; j < want.steps.length; ++j) {
                assertEq(uint8(got.steps[j].op), uint8(want.steps[j].op), "step op");
                assertEq(got.steps[j].data, want.steps[j].data, "step data");
            }
            assertEq(got.cycleSlippageBps, want.cycleSlippageBps, "cycleSlippageBps");
            assertEq(got.minProfit, want.minProfit, "minProfit");
            assertEq(got.declaredResidue, want.declaredResidue, "declaredResidue");
            assertEq(got.commitment, want.commitment, "commitment");
            assertEq(got.chainId, want.chainId, "chainId");
            assertEq(got.deadline, want.deadline, "deadline");
        }
    }

    /// External so the selector can be stripped with calldata slicing.
    function decodeStartV2(bytes calldata data)
        external
        pure
        returns (MultiVenueArbImplementation.PlanV2 memory)
    {
        assert(bytes4(data[:4]) == MultiVenueArbImplementation.startV2.selector);
        return abi.decode(data[4:], (MultiVenueArbImplementation.PlanV2));
    }

    /// Prints the current encodings, for the deliberate regeneration above.
    function testPrintStartV2Calldata() external pure {
        console.log("selector", vm.toString(abi.encodePacked(MultiVenueArbImplementation.startV2.selector)));
        string[3] memory names = _names();
        for (uint256 i; i < names.length; ++i) {
            console.log(names[i], vm.toString(_encoded(i)));
        }
    }
}
