// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

contract TestBase {
    event Log(string message);

    function assertTrue(bool condition, string memory message) internal {
        if (!condition) {
            revert(message);
        }
    }

    function assertEq(uint256 a, uint256 b, string memory message) internal {
        if (a != b) {
            revert(message);
        }
    }

    function assertGt(uint256 a, uint256 b, string memory message) internal {
        if (a <= b) {
            revert(message);
        }
    }

    function assertEq(address a, address b, string memory message) internal {
        if (a != b) {
            revert(message);
        }
    }
}
