// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Test} from "forge-std/Test.sol";
import {MockERC20} from "../contracts/mocks/MockERC20.sol";
import {MockMorphoFlash} from "../contracts/mocks/MockMorphoFlash.sol";
import {MorphoFlashLender} from "../contracts/lenders/MorphoFlashLender.sol";

contract Borrower {
    enum Mode { Good, WrongReturn, NoApprove, Reenter }
    Mode public mode;
    address public seenInitiator;
    address public seenToken;
    uint256 public seenAmount;
    uint256 public seenFee;
    bytes public seenData;
    uint256 public heldDuring;

    function setMode(Mode m) external { mode = m; }

    function borrow(MorphoFlashLender lender, address token, uint256 amount, bytes calldata data) external returns (bool) {
        return lender.flashLoan(address(this), token, amount, data);
    }

    function onFlashLoan(address initiator, address token, uint256 amount, uint256 fee, bytes calldata data)
        external
        returns (bytes32)
    {
        seenInitiator = initiator;
        seenToken = token;
        seenAmount = amount;
        seenFee = fee;
        seenData = data;
        heldDuring = MockERC20(token).balanceOf(address(this));
        if (mode == Mode.Reenter) {
            mode = Mode.Good;
            MorphoFlashLender(msg.sender).flashLoan(address(this), token, amount, data);
        }
        if (mode != Mode.NoApprove) {
            MockERC20(token).approve(msg.sender, amount + fee);
        }
        if (mode == Mode.WrongReturn) return bytes32(uint256(1));
        return keccak256("ERC3156FlashBorrower.onFlashLoan");
    }
}

/// A token whose calls return false instead of reverting.
contract FalseToken {
    function balanceOf(address) external pure returns (uint256) { return type(uint128).max; }
    function transfer(address, uint256) external pure returns (bool) { return false; }
    function transferFrom(address, address, uint256) external pure returns (bool) { return false; }
    function approve(address, uint256) external pure returns (bool) { return false; }
}

contract MorphoFlashLenderTest is Test {
    MockERC20 internal weth;
    MorphoFlashLender internal lender;
    Borrower internal borrower;
    address internal morpho;

    function setUp() public {
        weth = new MockERC20("Wrapped Ether", "WETH", 18);
        lender = new MorphoFlashLender();
        morpho = lender.MORPHO();
        vm.etch(morpho, address(new MockMorphoFlash()).code);
        weth.mint(morpho, 1_000 ether);
        borrower = new Borrower();
    }

    function test_a_loan_round_trips_free_and_leaves_nothing() public {
        assertTrue(borrower.borrow(lender, address(weth), 500 ether, hex"c0ffee"));
        assertEq(borrower.seenInitiator(), address(borrower), "the initiator is whoever called flashLoan");
        assertEq(borrower.seenToken(), address(weth));
        assertEq(borrower.seenAmount(), 500 ether);
        assertEq(borrower.seenFee(), 0);
        assertEq(borrower.seenData(), hex"c0ffee");
        assertEq(borrower.heldDuring(), 500 ether, "the borrower held the loan in its callback");
        assertEq(weth.balanceOf(morpho), 1_000 ether, "Morpho was repaid");
        assertEq(weth.balanceOf(address(lender)), 0, "the lender holds nothing");
        assertEq(weth.allowance(address(lender), morpho), type(uint256).max, "Morpho is approved once, for good");
        // A second loan runs, and needs no new approval.
        assertTrue(borrower.borrow(lender, address(weth), 1 ether, ""));
        assertEq(weth.balanceOf(morpho), 1_000 ether);
    }


    /// ERC-3156: the initiator is whoever called `flashLoan`, and the receiver
    /// the borrower it named. The executor checks the initiator is itself.
    function test_the_initiator_is_the_caller_and_the_borrower_receives() public {
        assertTrue(lender.flashLoan(address(borrower), address(weth), 2 ether, ""));
        assertEq(borrower.seenInitiator(), address(this));
        assertEq(borrower.heldDuring(), 2 ether);
        assertEq(weth.balanceOf(address(this)), 0, "the caller never held the loan");
    }

    function test_the_terms_are_morphos() public view {
        assertEq(lender.flashFee(address(weth), 123 ether), 0);
        assertEq(lender.maxFlashLoan(address(weth)), 1_000 ether);
    }

    function test_only_morpho_calls_back() public {
        vm.expectRevert(MorphoFlashLender.NotMorpho.selector);
        lender.onMorphoFlashLoan(1 ether, abi.encode(address(borrower), address(borrower), address(weth), bytes("")));
    }

    /// A callback with no loan behind it has nothing to send: it moves nothing.
    function test_a_callback_without_a_loan_moves_nothing() public {
        vm.prank(morpho);
        vm.expectRevert(MorphoFlashLender.TokenCallFailed.selector);
        lender.onMorphoFlashLoan(1 ether, abi.encode(address(borrower), address(borrower), address(weth), bytes("")));
    }

    function test_a_borrower_must_answer_the_standard_value() public {
        borrower.setMode(Borrower.Mode.WrongReturn);
        vm.expectRevert(MorphoFlashLender.CallbackFailed.selector);
        borrower.borrow(lender, address(weth), 1 ether, "");
    }

    function test_a_borrower_that_does_not_repay_reverts_the_loan() public {
        borrower.setMode(Borrower.Mode.NoApprove);
        vm.expectRevert(MorphoFlashLender.TokenCallFailed.selector);
        borrower.borrow(lender, address(weth), 1 ether, "");
    }

    function test_a_token_call_that_returns_false_reverts_the_loan() public {
        FalseToken f = new FalseToken();
        vm.expectRevert(MorphoFlashLender.TokenCallFailed.selector);
        borrower.borrow(lender, address(f), 1 ether, "");
    }

    /// A loan inside a loan is another Morpho loan, repaid on its own.
    function test_a_loan_inside_a_loan_is_repaid_on_its_own() public {
        borrower.setMode(Borrower.Mode.Reenter);
        assertTrue(borrower.borrow(lender, address(weth), 1 ether, ""));
        assertEq(weth.balanceOf(morpho), 1_000 ether);
        assertEq(weth.balanceOf(address(lender)), 0);
    }
}
