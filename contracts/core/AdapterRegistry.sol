// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

error UnknownAdapter(uint16 adapterId);
error InvalidAdapter();
error AdapterAlreadyRegistered(uint16 adapterId);

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

    event AdapterRegistered(uint16 indexed adapterId, address indexed adapter);
    event AdapterDeregistered(uint16 indexed adapterId, address indexed adapter);

    /// The address a step naming `adapterId` will reach, or zero.
    function adapterOf(uint16 adapterId) external view returns (address) {
        return adapters[adapterId];
    }

    /// Resolve or revert. The only way a target enters an execution path.
    function _resolveAdapter(uint16 adapterId) internal view returns (address adapter) {
        adapter = adapters[adapterId];
        if (adapter == address(0)) revert UnknownAdapter(adapterId);
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
