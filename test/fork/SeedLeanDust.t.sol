// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {LeanExecutor} from "../../contracts/lean/LeanExecutor.sol";
import {SeedLeanDust} from "../../script/SeedLeanDust.s.sol";

interface IERC20S {
    function balanceOf(address) external view returns (uint256);
}

/// Task 8.5 R29, on a fork of Base: the dust seeding buys a few units of every
/// middle token the canary's universe trades through (41 pools, 13 tokens),
/// each through its pool's venue router, and leaves them at the executor, with
/// 1 wei of WETH there and 1 wei at the owner.
///
///   FOUNDRY_PROFILE=fork forge test --match-path test/fork/SeedLeanDust.t.sol -vv
contract SeedLeanDustForkTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address owner = address(0xA11CE);

    function setUp() public {
        vm.createSelectFork("https://base.drpc.org", 52_671_989);
    }

    function testEveryMiddleTokenIsSeeded() public {
        LeanExecutor lean = new LeanExecutor(owner, address(0xB0B), owner);
        (address[] memory tokens, uint8[] memory kinds, address[] memory pools) = universe();
        vm.deal(owner, 1 ether);
        SeedLeanDust script = new SeedLeanDust();
        script.seedAs(owner, address(lean), tokens, kinds, pools);
        for (uint256 i; i < tokens.length; i++) {
            assertGt(IERC20S(tokens[i]).balanceOf(address(lean)), 0, vm.toString(tokens[i]));
        }
        assertEq(IERC20S(WETH).balanceOf(address(lean)), 1, "1 wei of WETH at the executor");
        assertEq(IERC20S(WETH).balanceOf(owner), 1, "1 wei of WETH kept at the owner");
    }

    /// Each middle token and the pool it is bought through: its deepest Uniswap
    /// pool where the universe has one, else its deepest pool on another venue.
    function universe() internal pure returns (address[] memory tokens, uint8[] memory kinds, address[] memory pools) {
        tokens = new address[](13);
        kinds = new uint8[](13);
        pools = new address[](13);
        (tokens[0], kinds[0], pools[0]) = (0x04C0599Ae5A44757c0af6F9eC3b93da8976c150A, 2, 0xbD3cd0D9d429b41F0a2e1C026552Bd598294d5E0);
        (tokens[1], kinds[1], pools[1]) = (0x0b3e328455c4059EEb9e3f84b5543F74E24e7E1b, 0, 0x9c087Eb773291e50CF6c6a90ef0F4500e349B903);
        (tokens[2], kinds[2], pools[2]) = (0x2Ae3F1Ec7F1F5012CFEab0185bfc7aa3cf0DEc22, 2, 0x47cA96Ea59C13F72745928887f84C9F52C3D7348);
        (tokens[3], kinds[3], pools[3]) = (0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913, 0, 0xd0b53D9277642d899DF5C87A3966A349A798F224);
        (tokens[4], kinds[4], pools[4]) = (0x940181a94A35A4569E4529A3CDfB74e38FD98631, 0, 0x3d5D143381916280ff91407FeBEB52f2b60f33Cf);
        (tokens[5], kinds[5], pools[5]) = (0x9B8Df6E244526ab5F6e6400d331DB28C8fdDdb55, 0, 0xab04A352D4EC8d0F1812A4E5EBe7807Fa67acD48);
        (tokens[6], kinds[6], pools[6]) = (0xA4A2E2ca3fBfE21aed83471D28b6f65A233C6e00, 3, 0x0E4409F7a8ebA3213F336daf608985302e79328c);
        (tokens[7], kinds[7], pools[7]) = (0xacfE6019Ed1A7Dc6f7B508C02d1b04ec88cC21bf, 2, 0x7eC6C9D993D9832Aa654593f2Dbc21303650Bc6c);
        (tokens[8], kinds[8], pools[8]) = (0xBAa5CC21fd487B8Fcc2F632f3F4E8D37262a0842, 0, 0x2F42Df4aF5312B492E9d7F7b2110D9c7bf2D9e4F);
        (tokens[9], kinds[9], pools[9]) = (0xc1CBa3fCea344f92D9239c08C0568f6F2F0ee452, 2, 0x861A2922bE165a5Bd41b1E482B49216b465e1B5F);
        (tokens[10], kinds[10], pools[10]) = (0xCb327b99fF831bF8223cCEd12B1338FF3aA322Ff, 2, 0x2ae9DF02539887d4EbcE0230168a302d34784c82);
        (tokens[11], kinds[11], pools[11]) = (0xcb585250f852C6c6bf90434AB21A00f02833a4af, 3, 0xB90fe999Be6869AF0aFC557DCCfBE169EA3403D6);
        (tokens[12], kinds[12], pools[12]) = (0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf, 0, 0x7AeA2E8A3843516afa07293a10Ac8E49906dabD1);
    }
}
