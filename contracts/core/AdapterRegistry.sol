// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

error UnknownAdapter(uint16 adapterId);
error InvalidAdapter();
error AdapterAlreadyRegistered(uint16 adapterId);
error SelectorNotAllowed(uint16 adapterId, bytes4 selector);
error EmptyCallData();

/// The allowlist that replaces `_execGeneric`'s caller-supplied target (B-1).
///
/// # The one idea
///
/// A settlement step names an adapter by **id**; the contract resolves the id
/// to an address it was told about in advance. The caller never supplies a
/// target. That is the entire fix, and everything else here exists to stop the
/// idea being undone by a side door.
///
/// # Why registration is `onlyOwner` and not `onlyExecutor`
///
/// If the key that submits trades could also register adapters, it could
/// register an attacker and the allowlist would be decoration. The whole point
/// of B-1 is that **an executor is authorised to trade, not to choose what the
/// contract may call**, so those two authorities are held by different roles.
/// `configAdmin` is deliberately not enough either: registering an adapter
/// grants call authority over the contract's holdings, which is closer to
/// ownership than to configuration.
///
/// # Why an id may never be silently re-pointed
///
/// `register` refuses an id that is already taken. Re-pointing an adapter id
/// is a substitution attack in one transaction — every plan already encoded
/// against that id now calls somewhere else, with no change to any plan and no
/// signal to anything that validated one. Replacing an adapter is therefore
/// two deliberate steps, `deregister` then `register`, and the first emits an
/// event that a monitor can act on.
abstract contract AdapterRegistry {
    /// `adapterId` → the only address a step naming that id may reach.
    ///
    /// Storage lives in the inheriting contract. This is a base rather than a
    /// separate deployment so that resolving an id costs an `SLOAD` rather
    /// than an external call on the settlement path.
    mapping(uint16 => address) internal adapters;

    /// `adapterId` → selector → allowed.
    ///
    /// An adapter is a contract, and a contract has more functions than the
    /// one a route needs. Resolving the *address* stops a plan calling
    /// anywhere; allowing the *selector* stops it calling an adapter's admin
    /// surface, its upgrade hook, or a `transfer` it happens to expose. INV-26
    /// asks for target, selector, pool and token — target and selector are
    /// what the executor can check without knowing venue semantics, and the
    /// last two belong to the adapter, which is the only thing that knows what
    /// a valid pool for its venue looks like.
    mapping(uint16 => mapping(bytes4 => bool)) internal allowedSelectors;

    event AdapterRegistered(uint16 indexed adapterId, address indexed adapter);
    event AdapterDeregistered(uint16 indexed adapterId, address indexed adapter);
    event SelectorAllowed(uint16 indexed adapterId, bytes4 indexed selector);
    event SelectorRevoked(uint16 indexed adapterId, bytes4 indexed selector);

    /// The address a step naming `adapterId` will reach, or zero.
    function adapterOf(uint16 adapterId) external view returns (address) {
        return adapters[adapterId];
    }

    /// Resolve or revert. The only way a target enters an execution path.
    function _resolveAdapter(uint16 adapterId) internal view returns (address adapter) {
        adapter = adapters[adapterId];
        if (adapter == address(0)) revert UnknownAdapter(adapterId);
    }

    /// Whether a step may invoke `selector` on `adapterId`.
    function isSelectorAllowed(uint16 adapterId, bytes4 selector) external view returns (bool) {
        return allowedSelectors[adapterId][selector];
    }

    /// Check the call a step is about to make, or revert naming what was
    /// refused.
    ///
    /// Empty calldata is refused outright: it invokes the adapter's `receive`
    /// or `fallback`, which is a function nobody allowlisted and which a
    /// selector check cannot see.
    function _requireAllowedCall(uint16 adapterId, bytes memory callData) internal view {
        if (callData.length < 4) revert EmptyCallData();
        // No assembly. This was `mload(add(callData, 32))`, which is the
        // idiomatic way to read a selector and is also unreadable to every
        // static analyser -- including the one that flagged it here, whose
        // note was "static analysis modules do not parse inline Assembly, this
        // can lead to wrong analysis results". This function is the allowlist
        // enforcement that closes B-1, so it is the single worst place in the
        // codebase for a tool, or a reviewer, to have to skip.
        //
        // The cast truncates, which is what `forge-lint` objects to; the
        // length check above is what makes that safe, and it is the same guard
        // the assembly needed. `test/SelectorExtraction.t.sol` holds the old
        // implementation and fuzzes the two against each other, so the
        // equivalence is a checked property rather than a claim made once.
        // forge-lint: disable-next-line(unsafe-typecast)
        bytes4 selector = bytes4(callData);
        if (!allowedSelectors[adapterId][selector]) {
            revert SelectorNotAllowed(adapterId, selector);
        }
    }

    function _allowSelector(uint16 adapterId, bytes4 selector) internal {
        if (adapters[adapterId] == address(0)) revert UnknownAdapter(adapterId);
        allowedSelectors[adapterId][selector] = true;
        emit SelectorAllowed(adapterId, selector);
    }

    function _revokeSelector(uint16 adapterId, bytes4 selector) internal {
        delete allowedSelectors[adapterId][selector];
        emit SelectorRevoked(adapterId, selector);
    }

    function _registerAdapter(uint16 adapterId, address adapter) internal {
        if (adapter == address(0)) revert InvalidAdapter();
        // An adapter with no code is an address that was never deployed, or
        // one whose deployment is still ahead of us. Either way a step naming
        // it would `safeCall` into nothing and succeed silently, which is the
        // failure mode a registry exists to prevent.
        if (adapter.code.length == 0) revert InvalidAdapter();
        if (adapters[adapterId] != address(0)) revert AdapterAlreadyRegistered(adapterId);
        adapters[adapterId] = adapter;
        emit AdapterRegistered(adapterId, adapter);
    }

    function _deregisterAdapter(uint16 adapterId) internal {
        address previous = adapters[adapterId];
        if (previous == address(0)) revert UnknownAdapter(adapterId);
        delete adapters[adapterId];
        emit AdapterDeregistered(adapterId, previous);
    }
}
