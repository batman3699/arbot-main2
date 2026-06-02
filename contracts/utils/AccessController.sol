// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

abstract contract AccessController {
    event RoleGranted(bytes32 indexed role, address indexed account, address indexed sender);
    event RoleRevoked(bytes32 indexed role, address indexed account, address indexed sender);

    bytes32 public constant DEFAULT_ADMIN_ROLE = 0x00;

    mapping(bytes32 => mapping(address => bool)) private _roles;
    mapping(bytes32 => bytes32) private _roleAdmins;

    modifier onlyRole(bytes32 role) {
        _checkRole(role, msg.sender);
        _;
    }

    constructor() {
        _grantRole(DEFAULT_ADMIN_ROLE, msg.sender);
    }

    function hasRole(bytes32 role, address account) public view returns (bool) {
        return _roles[role][account];
    }

    function getRoleAdmin(bytes32 role) public view returns (bytes32) {
        bytes32 admin = _roleAdmins[role];
        return admin == bytes32(0) ? DEFAULT_ADMIN_ROLE : admin;
    }

    function grantRole(bytes32 role, address account) public onlyRole(getRoleAdmin(role)) {
        _grantRole(role, account);
    }

    function revokeRole(bytes32 role, address account) public onlyRole(getRoleAdmin(role)) {
        if (!_roles[role][account]) return;
        _roles[role][account] = false;
        emit RoleRevoked(role, account, msg.sender);
    }

    function renounceRole(bytes32 role, address account) public {
        if (account != msg.sender) revert("AccessController: can only renounce roles for self");
        if (!_roles[role][account]) return;
        _roles[role][account] = false;
        emit RoleRevoked(role, account, msg.sender);
    }

    function _checkRole(bytes32 role, address account) internal view {
        if (!hasRole(role, account)) {
            revert("AccessController: missing role");
        }
    }

    function _grantRole(bytes32 role, address account) internal {
        if (_roles[role][account]) return;
        _roles[role][account] = true;
        emit RoleGranted(role, account, msg.sender);
    }

    function _setRoleAdmin(bytes32 role, bytes32 adminRole) internal {
        _roleAdmins[role] = adminRole;
    }
}
