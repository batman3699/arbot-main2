// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Test} from "forge-std/Test.sol";
import {AdapterRegistry, EmptyCallData, SelectorNotAllowed} from "../contracts/core/AdapterRegistry.sol";

/// Exposes the registry's selector check, and keeps a reference implementation
/// of the assembly it replaced.
contract SelectorHarness is AdapterRegistry {
    function allow(uint16 id, address adapter, bytes4 sel) external {
        _registerAdapter(id, adapter);
        _allowSelector(id, sel);
    }

    /// The production path.
    function check(uint16 id, bytes memory callData) external view {
        _requireAllowedCall(id, callData);
    }

    /// **The implementation this replaced**, kept so the equivalence is a
    /// checked property rather than an argument made once in a commit message.
    ///
    /// `mload` reads 32 bytes from the start of the data and assignment to a
    /// `bytes4` keeps the high-order four. `bytes4(callData)` takes the first
    /// four and zero-pads a shorter input. Those agree for every input of
    /// length >= 4, which is the only shape `_requireAllowedCall` ever sees --
    /// and this asserts it over arbitrary bytes rather than asserting it.
    function selectorViaAssembly(bytes memory callData) public pure returns (bytes4 s) {
        assembly {
            s := mload(add(callData, 32))
        }
    }

    function selectorViaCast(bytes memory callData) public pure returns (bytes4) {
        // forge-lint: disable-next-line(unsafe-typecast)
        return bytes4(callData);
    }
}

/// The SolidityScan report flagged `AdapterRegistry.sol:87` — *"the contract
/// uses inline assembly ... static analysis modules do not parse inline
/// Assembly, this can lead to wrong analysis results."*
///
/// That is a statement about the scanner rather than a vulnerability, and it is
/// the most useful sentence in the report: the assembly sat inside
/// `_requireAllowedCall`, the allowlist enforcement that closes B-1. The tool
/// was saying it could not read the one function that matters most — and a
/// human auditor reads assembly more slowly and more suspiciously too.
contract SelectorExtractionTest is Test {
    SelectorHarness internal h;
    address internal adapter;

    function setUp() public {
        h = new SelectorHarness();
        adapter = address(new SelectorHarness()); // any address with code
    }

    /// The equivalence, over arbitrary calldata of any length >= 4.
    function testFuzzCastMatchesAssemblyForEveryValidLength(bytes memory data) public view {
        vm.assume(data.length >= 4);
        assertEq(h.selectorViaCast(data), h.selectorViaAssembly(data), "selector extraction diverged");
    }

    /// And at the boundary the guard admits: exactly four bytes, nothing after.
    function testExactlyFourBytesAgree() public view {
        bytes memory four = hex"a9059cbb";
        assertEq(h.selectorViaCast(four), bytes4(0xa9059cbb));
        assertEq(h.selectorViaCast(four), h.selectorViaAssembly(four));
    }

    /// A real selector out of a real encoded call.
    function testASelectorFromAnEncodedCallIsRecovered() public view {
        bytes memory encoded = abi.encodeWithSignature("donate(address,uint256)", address(1), 2);
        assertEq(h.selectorViaCast(encoded), bytes4(keccak256("donate(address,uint256)")));
        assertEq(h.selectorViaCast(encoded), h.selectorViaAssembly(encoded));
    }

    /// **Where the two forms differ, and why it cannot matter.** Below four
    /// bytes the cast zero-pads and the assembly reads whatever follows the
    /// data in memory. `_requireAllowedCall` reverts before either runs, which
    /// is what makes the substitution safe rather than merely equivalent.
    function testShortCalldataIsRefusedBeforeEitherFormRuns() public {
        h.allow(1, adapter, bytes4(0xa9059cbb));
        for (uint256 len = 0; len < 4; len++) {
            bytes memory short = new bytes(len);
            vm.expectRevert(EmptyCallData.selector);
            h.check(1, short);
        }
    }

    /// The check still does its job: an allowed selector passes, a neighbouring
    /// one does not.
    function testTheAllowlistStillGates() public {
        bytes4 allowed = bytes4(keccak256("donate(address,uint256)"));
        h.allow(1, adapter, allowed);

        h.check(1, abi.encodeWithSelector(allowed, address(1), 2));

        bytes4 other = bytes4(keccak256("drain(address,uint256)"));
        vm.expectRevert(abi.encodeWithSelector(SelectorNotAllowed.selector, uint16(1), other));
        h.check(1, abi.encodeWithSelector(other, address(1), 2));
    }

    /// Trailing argument data must not shift the selector -- the failure mode
    /// where an attacker pads a call to slide a different selector into view.
    function testFuzzTrailingDataNeverChangesTheSelector(bytes memory tail) public view {
        bytes4 sel = bytes4(keccak256("donate(address,uint256)"));
        bytes memory data = abi.encodePacked(sel, tail);
        assertEq(h.selectorViaCast(data), sel);
        assertEq(h.selectorViaAssembly(data), sel);
    }
}
