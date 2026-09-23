// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {AdapterRegistry} from "../../contracts/core/AdapterRegistry.sol";
import {Test} from "forge-std/Test.sol";

/// A minimal contract that exists only to have code, so `_registerAdapter`'s
/// code-length check can pass.
contract Adapterish {
    function anything() external {}
}

/// The registry with its internals exposed, plus a shadow model the invariant
/// compares against.
contract RegistryHarness is AdapterRegistry {
    /// What the harness *believes* is registered, maintained independently of
    /// the contract. The invariant is that the two never disagree — a shadow
    /// model is how a stateful fuzz test says "and it still means what it
    /// meant" rather than merely "it did not revert".
    mapping(uint16 => address) public shadowAdapter;
    mapping(uint16 => mapping(bytes4 => bool)) public shadowSelector;

    uint16[] public touchedIds;
    mapping(uint16 => bool) internal seen;

    Adapterish public immutable filler;

    constructor() {
        filler = new Adapterish();
    }

    /// Ids are folded into a small space, and that is about test QUALITY
    /// before it is about speed.
    ///
    /// Over the full 65,536 ids a fuzzer almost never registers one that is
    /// already taken, so the collision paths -- `AdapterAlreadyRegistered`,
    /// deregister-then-register, allow-on-a-deregistered-id -- would go
    /// unexercised while the run looked thorough. Eight ids makes collisions
    /// the common case. It also keeps `touchedIds` bounded, which matters
    /// because the invariants scan it and an unbounded set makes each check
    /// O(depth).
    function _fold(uint16 id) private pure returns (uint16) {
        return id % 8;
    }

    function _touch(uint16 id) private {
        if (!seen[id]) {
            seen[id] = true;
            touchedIds.push(id);
        }
    }

    function touchedCount() external view returns (uint256) {
        return touchedIds.length;
    }

    // ---- the operations the fuzzer may perform -----------------------------

    function register(uint16 id) external {
        id = _fold(id);
        _touch(id);
        // The fuzzer is allowed to attempt an invalid operation; what must
        // hold is that a refused operation changes nothing.
        try this.externalRegister(id, address(filler)) {
            shadowAdapter[id] = address(filler);
        } catch {}
    }

    function registerNoCode(uint16 id, address eoa) external {
        id = _fold(id);
        _touch(id);
        try this.externalRegister(id, eoa) {
            shadowAdapter[id] = eoa;
        } catch {}
    }

    function deregister(uint16 id) external {
        id = _fold(id);
        _touch(id);
        try this.externalDeregister(id) {
            shadowAdapter[id] = address(0);
            // Selectors deliberately survive deregistration in the contract;
            // the shadow mirrors that so the invariant checks what IS, not
            // what would be tidy.
        } catch {}
    }

    function allow(uint16 id, bytes4 selector) external {
        id = _fold(id);
        _touch(id);
        try this.externalAllow(id, selector) {
            shadowSelector[id][selector] = true;
        } catch {}
    }

    function revoke(uint16 id, bytes4 selector) external {
        id = _fold(id);
        _touch(id);
        this.externalRevoke(id, selector);
        shadowSelector[id][selector] = false;
    }

    // `try` needs an external call, so each operation has a public wrapper.
    function externalRegister(uint16 id, address adapter) external {
        _registerAdapter(id, adapter);
    }

    function externalDeregister(uint16 id) external {
        _deregisterAdapter(id);
    }

    function externalAllow(uint16 id, bytes4 selector) external {
        _allowSelector(id, selector);
    }

    function externalRevoke(uint16 id, bytes4 selector) external {
        _revokeSelector(id, selector);
    }

    /// `try`/`catch` rather than `vm.expectRevert` in the invariants.
    ///
    /// `expectRevert` is a cheatcode with per-call semantics, and using it
    /// inside a loop inside an invariant made a refusal read as a pass-through
    /// -- the suite reported "next call did not revert as expected" on a call
    /// that does revert. Asking the question in Solidity has no ordering
    /// subtleties to get wrong.
    function resolveReverts(uint16 id) external view returns (bool) {
        try this.resolve(id) returns (address) {
            return false;
        } catch {
            return true;
        }
    }

    function allowedCallReverts(uint16 id, bytes memory callData) external view returns (bool) {
        try this.requireAllowed(id, callData) {
            return false;
        } catch {
            return true;
        }
    }

    function resolve(uint16 id) external view returns (address) {
        return _resolveAdapter(id);
    }

    function requireAllowed(uint16 id, bytes memory callData) external view {
        _requireAllowedCall(id, callData);
    }
}

/// Task 5.6 — `AdapterRegistry` under an arbitrary sequence of operations.
///
/// The properties here are the ones B-1's fix rests on. A unit test shows they
/// hold for a sequence somebody thought of; this shows they hold for sequences
/// nobody did.
contract AdapterRegistryInvariant is Test {
    RegistryHarness internal harness;

    function setUp() public {
        harness = new RegistryHarness();
        targetContract(address(harness));

        // Only the operations, not the `external*` wrappers those operations
        // call through — otherwise the fuzzer drives the raw mutators and the
        // shadow model never sees them.
        bytes4[] memory selectors = new bytes4[](5);
        selectors[0] = RegistryHarness.register.selector;
        selectors[1] = RegistryHarness.registerNoCode.selector;
        selectors[2] = RegistryHarness.deregister.selector;
        selectors[3] = RegistryHarness.allow.selector;
        selectors[4] = RegistryHarness.revoke.selector;
        targetSelector(FuzzSelector({addr: address(harness), selectors: selectors}));
    }

    /// The registry agrees with the shadow model after every sequence.
    ///
    /// This is the one that catches a refused operation that changed something
    /// anyway — `register` on a taken id, `deregister` on an empty one,
    /// `allow` on an unregistered adapter. Each must be a no-op, and "no-op"
    /// is only checkable against a model kept separately.
    function invariant_registryMatchesTheShadowModel() public view {
        uint256 n = harness.touchedCount();
        for (uint256 i; i < n; ++i) {
            uint16 id = harness.touchedIds(i);
            assertEq(
                harness.adapterOf(id),
                harness.shadowAdapter(id),
                "the registry and the model disagree about an adapter"
            );
        }
    }

    /// An id with no adapter never resolves, whatever happened before.
    ///
    /// This is B-1's fix stated as a property: a step naming an unregistered
    /// adapter cannot reach anything.
    function invariant_anUnregisteredIdNeverResolves() public view {
        uint256 n = harness.touchedCount();
        for (uint256 i; i < n; ++i) {
            uint16 id = harness.touchedIds(i);
            if (harness.adapterOf(id) == address(0)) {
                assertTrue(harness.resolveReverts(id), "an unregistered id resolved");
            }
        }
    }

    /// No address without code is ever registered.
    ///
    /// A step naming one would `safeCall` into nothing and succeed silently,
    /// which is the failure the allowlist exists to prevent — and the fuzzer
    /// is given `registerNoCode` specifically so it keeps trying.
    function invariant_nothingWithoutCodeIsEverRegistered() public view {
        uint256 n = harness.touchedCount();
        for (uint256 i; i < n; ++i) {
            address adapter = harness.adapterOf(harness.touchedIds(i));
            if (adapter != address(0)) {
                assertGt(adapter.code.length, 0, "an address with no code is registered");
            }
        }
    }

    /// Empty calldata is refused for every id, registered or not. There is no
    /// sequence of operations that allowlists a `fallback`.
    function invariant_emptyCallDataIsNeverAllowed() public view {
        uint256 n = harness.touchedCount();
        for (uint256 i; i < n; ++i) {
            assertTrue(
                harness.allowedCallReverts(harness.touchedIds(i), bytes("")),
                "empty calldata was allowed"
            );
        }
    }

    /// A selector the model says is not allowed is refused by the contract.
    ///
    /// The other half of the shadow comparison: the first invariant checks
    /// adapters agree, this checks selectors do — and it is the one that would
    /// catch `revoke` leaving a selector allowed, or `allow` succeeding on an
    /// id that was never registered.
    function invariant_selectorsMatchTheShadowModel() public view {
        uint256 n = harness.touchedCount();
        bytes4 probe = bytes4(0xdeadbeef);
        for (uint256 i; i < n; ++i) {
            uint16 id = harness.touchedIds(i);
            assertEq(
                harness.isSelectorAllowed(id, probe),
                harness.shadowSelector(id, probe),
                "the registry and the model disagree about a selector"
            );
        }
    }
}
