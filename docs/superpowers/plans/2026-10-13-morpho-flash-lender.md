# Morpho Flash Lender Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Size every trade, in both lanes, up to Morpho Blue's WETH holding, borrowed free through a small ERC-3156 lender contract, instead of stopping at Balancer's ~19 WETH. The deployed executor does not change.

**Architecture:**
- A ~60-line immutable `MorphoFlashLender` turns Morpho Blue's `flashLoan`/`onMorphoFlashLoan` into ERC-3156. The deployed executor's ERC-3156 branch already settles that correctly.
- It is proven on Base by `eth_simulateV1` through the real executor, with its runtime code injected by state override, before anyone deploys it.
- In Rust, a `Lenders` value (Balancer's holding, plus Morpho's when configured) sets the sizing cap and each size's lender: Balancer while it covers the size, Morpho above.
  - The reactive lane carries the choice economics → commitment → call builder → reader.
  - The armed lane carries it Pricing → `armed_call` → the arm line.

**Tech Stack:** Solidity 0.8.21 (Foundry, via-IR, forge-std only); Rust (apex-runtime, apex-exec, apex-chain); Base mainnet reads through BlockPI.

**Spec:** `docs/superpowers/specs/2026-10-13-morpho-flash-lender-design.md` (`e122cb3`).

## Changes found while executing (2026-10-13)

1. **The lender keeps no state, and approves Morpho once for good.**
   - Morpho Blue calls back only the caller of its `flashLoan`, so a callback from Morpho is for a loan this contract started, and the borrower and initiator can travel in the callback data.
   - Dropping the two storage writes (and their clears), and approving `type(uint256).max` once instead of each loan, cut Morpho's extra gas over Balancer from **148,053 to 94,167**.
   - Gas divides the bid in a priority auction, and the contract is immutable once deployed. The `LoanInFlight`/`NoLoan` guards went with the state.
   - Tests: a nested loan is repaid on its own; a callback with no loan behind it moves nothing; the initiator is the caller, not the borrower. 9 tests, 8 mutants killed.
2. **The proof quotes on the replay's own state.**
   - At block 52,526,060 three replayed swaps on the 0.05% pool ran with different amounts from the chain's: they depend on transactions that fail in replay (346 of 902 did).
   - Quoted on the chain's logs, the 20 WETH gross was 1.086e13 wei above the simulation's. Quoted on the replay's own logs, it matches to the wei.
   - The proof also uses a 60 s request timeout: a spike block replayed to its trigger outlasts the default 3 s.
3. **Measured** (`PROOF 3`, ticket 51's cycle at 15 WETH): Balancer 461,224 gas, Morpho 555,391, so `MORPHO_EXTRA_GAS` = 95,000.
4. **The proof reads the deployed lender from the config** (`lenders.morpho`), not from an `APEX_MORPHO_LENDER` variable: `env_coverage` refuses a new non-secret variable (blueprint §2.4, no late configuration lookup).
5. **The evaluator compares a simulation against a quote on the replay's own state when the replay diverges from the chain** (`replayed_logs`, `replay_diverged`, `SimCheck.replay_diverged`). It also waits up to `EVAL_REQUEST_TIMEOUT` (60 s) for an answer.
   - Morpho-sized fires happen in spike blocks, where replays diverge. Without this, R27's own success criterion (Morpho fires agreeing with their quotes) would fail on replay artefacts.

## Global Constraints

- **Secrets:**
  - Never print, echo or log a secret (`APEX_SECRET_*`, `BLOCKPI_KEY`, `PRIVATE_KEY`, `TRADER_PRIVATE_KEY`).
  - Pass keys by environment or `curl -K -` only, never on a command line.
  - Do not modify `.env`.
- **Broadcasting:** never broadcast a mainnet transaction. The deploy is prepared and dry-run, and **the operator broadcasts it**.
- **Cargo:** never run `cargo fmt`. Use `-j 3` for every cargo command (15 GB machine).
- **Committing:**
  - Stage named paths only, never `git add -A`. After committing, `git diff --stat -- <paths>` must be empty.
  - Before committing, run all five gates: `bash $SCRATCH/gates.sh`, where `$SCRATCH` is this session's scratchpad. The known-red scripts are `check_placeholder_endpoints.sh` and `cl_parity_sweep.sh`.
  - Verify each commit in a clean worktree with its own `CARGO_TARGET_DIR`.
  - Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- **Mutation testing:** restore and `touch` after each mutant, and run with `--no-fail-fast`. A survivor gets a test, or its code is deleted if it cannot change a result.
- **Inventory:** `data/` (the pool inventory) exists only in the main tree. Tasks that load the universe run there.
- **Lender facts** (read 2026-10-13, checked against chain code):
  - Balancer V2 vault `0xBA12222222228d8Ba445958a75a0704d566BF2C8` holds ~18.98 WETH, fee 0.
  - Morpho Blue `0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb` (15,623 bytes; `flashLoan(address,uint256,bytes)` = `0xe0232b42`, callback `onMorphoFlashLoan(uint256,bytes)` = `0x31f57072`) holds ~82,679 WETH, fee 0.
  - Executor `0x8940B565D050974b2b589B70De43bCb753b1F93B`; trader `0xCB436Ba3acb945b3fc8EE6345857262584356595`; WETH `0x4200000000000000000000000000000000000006` (WETH9: `balanceOf` mapping at slot 3).
- **Never build an Aave loan** (`LoanProvider::Aave`) for this executor: it counts Aave's later pull as profit. This was proven by simulation on 2026-10-13.
- `FlashProviderId(1)` is Balancer (`BALANCER_FLASH`); `FlashProviderId(2)` is the Morpho lender (`MORPHO_FLASH`).

---

## File structure

| File | Responsibility |
|---|---|
| `contracts/lenders/MorphoFlashLender.sol` (new) | the ERC-3156 lender over Morpho Blue |
| `contracts/mocks/MockMorphoFlash.sol` (new) | Morpho Blue's flash-loan semantics, for tests |
| `test/MorphoFlashLender.t.sol` (new) | the lender's Foundry tests |
| `script/DeployMorphoFlashLender.s.sol` (new) | the deploy script the operator broadcasts |
| `crates/apex-runtime/src/live/lenders.rs` (new) | `Lenders`, `LenderChoice`, the size rule, `loan()` with the Aave refusal |
| `crates/apex-runtime/src/live/mod.rs` | `pub mod lenders;` |
| `crates/apex-runtime/src/live/pricing.rs` | `CostedCycle::lent`, the per-size extra gas, and `LenderHolding` carrying `Lenders` |
| `crates/apex-runtime/src/econ.rs` | several `FlashTerms`; `flash_quote` falls back to another lender that covers the amount |
| `crates/apex-runtime/src/live/calls.rs` | `LiveCallBuilder::with_morpho`; the loan from the commitment's lender |
| `crates/apex-runtime/src/live/reader.rs` | Morpho's holding in the view; `flash_available` by the ticket's lender |
| `crates/apex-runtime/src/armed/threshold.rs` | `Pricing.lenders` replaces `cap` |
| `crates/apex-runtime/src/armed/plan.rs` | `armed_call` takes the `LenderChoice` |
| `crates/apex-runtime/src/armed/engine.rs` | chooses the lender per rung size |
| `crates/apex-runtime/src/armed/lines.rs` | `Arm.lender` |
| `crates/apex-runtime/src/armed/run.rs` | `book_after` (the replayed book at a transaction), from `quote_at_fire` |
| `crates/apex-runtime/src/shadow/config.rs` | optional `lenders.morpho { address, code_hash }` |
| `crates/apex-runtime/src/shadow/mod.rs` | boot verification and read, terms, reader, builder, head loop, armed pricing, report |
| `scripts/shadow-status.sh`, `ops/shadow.base.yaml`, `ops/host/README.md`, `PLAN.md` | operations and record |
| `crates/apex-runtime/tests/morpho_lender_proof.rs` (new) | the on-chain proof (`#[ignore]`, network) |
| `crates/apex-runtime/tests/live_lenders.rs` (new) | the lender rule's tests |
| Tests extended | `live_pricing.rs`, `live_econ.rs`, `live_calls.rs`, `live_reader.rs`, `armed_engine.rs`, `armed_threshold.rs`, `armed_predicate.rs`, `shadow_parts.rs` |

---

### Task 1: `MorphoFlashLender` and its tests (Solidity)

**Files:**
- Create: `contracts/lenders/MorphoFlashLender.sol`, `contracts/mocks/MockMorphoFlash.sol`, `test/MorphoFlashLender.t.sol`

**Interfaces:**
- Produces:
  - `MorphoFlashLender` with `MORPHO()` (constant `0xBBBB…EEFFCb`) and `CALLBACK_SUCCESS()`;
  - `maxFlashLoan(address) → uint256` and `flashFee(address,uint256) → uint256`;
  - `flashLoan(address borrower, address token, uint256 amount, bytes data) → bool`, selector as `IERC3156FlashLender.flashLoan`;
  - `onMorphoFlashLoan(uint256,bytes)`;
  - errors `LoanInFlight`, `NotMorpho`, `NoLoan`, `CallbackFailed`, `TokenCallFailed`.
- The artifact `out/MorphoFlashLender.sol/MorphoFlashLender.json` (`deployedBytecode.object`) is what Task 2 injects.
- `MORPHO` is a `constant`, not an immutable: compiled runtime code must be complete to be injected by state override.

- [ ] **Step 1: Write the mock Morpho.** `contracts/mocks/MockMorphoFlash.sol`:

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {MockERC20} from "./MockERC20.sol";

interface IMorphoFlashLoanCallback {
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external;
}

/// Morpho Blue's `flashLoan`, as it behaves on Base: send the assets, call the
/// caller back, pull the assets back. Stateless, so `vm.etch` can put it at
/// Morpho's address.
contract MockMorphoFlash {
    function flashLoan(address token, uint256 assets, bytes calldata data) external {
        require(assets != 0, "zero assets");
        MockERC20(token).transfer(msg.sender, assets);
        IMorphoFlashLoanCallback(msg.sender).onMorphoFlashLoan(assets, data);
        MockERC20(token).transferFrom(msg.sender, address(this), assets);
    }
}
```

- [ ] **Step 2: Write the failing tests.** `test/MorphoFlashLender.t.sol`:

```solidity
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
        assertEq(weth.allowance(address(lender), morpho), 0, "no allowance is left behind");
        // The state is cleared: a second loan runs.
        assertTrue(borrower.borrow(lender, address(weth), 1 ether, ""));
    }

    function test_the_terms_are_morphos() public view {
        assertEq(lender.flashFee(address(weth), 123 ether), 0);
        assertEq(lender.maxFlashLoan(address(weth)), 1_000 ether);
    }

    function test_only_morpho_calls_back() public {
        vm.expectRevert(MorphoFlashLender.NotMorpho.selector);
        lender.onMorphoFlashLoan(1 ether, abi.encode(address(weth), bytes("")));
    }

    function test_morpho_calls_back_only_during_a_loan() public {
        vm.prank(morpho);
        vm.expectRevert(MorphoFlashLender.NoLoan.selector);
        lender.onMorphoFlashLoan(1 ether, abi.encode(address(weth), bytes("")));
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

    function test_no_loan_inside_a_loan() public {
        borrower.setMode(Borrower.Mode.Reenter);
        vm.expectRevert(MorphoFlashLender.LoanInFlight.selector);
        borrower.borrow(lender, address(weth), 1 ether, "");
    }
}
```

- [ ] **Step 3: Run it to see it fail.** Run: `forge test --match-contract MorphoFlashLenderTest`. Expected: a compile error, because `MorphoFlashLender.sol` does not exist.

- [ ] **Step 4: Write the lender.** `contracts/lenders/MorphoFlashLender.sol`:

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

interface IMorphoFlash {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

interface IERC20Calls {
    function balanceOf(address account) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function approve(address spender, uint256 amount) external returns (bool);
}

interface IERC3156Borrower {
    function onFlashLoan(address initiator, address token, uint256 amount, uint256 fee, bytes calldata data)
        external
        returns (bytes32);
}

/// Morpho Blue's flash loan as an ERC-3156 lender (Task 8.5 R27).
///
/// The deployed executor settles an ERC-3156 loan correctly: it repays
/// `amount + fee` by allowance after its callback. It cannot settle Aave's,
/// because it counts Aave's later pull as profit. Morpho Blue lends any token it
/// holds, free, through its own callback; this contract translates.
///
/// Immutable and ownerless. It holds a loan for one transaction and nothing
/// between. Anyone may borrow through it, as anyone may from Morpho directly.
contract MorphoFlashLender {
    /// Morpho Blue on Base: 15,623 bytes of code, `flashLoan` selector
    /// `0xe0232b42` (read 2026-10-13). A constant, so the compiled runtime code
    /// is the deployed code.
    address public constant MORPHO = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    bytes32 public constant CALLBACK_SUCCESS = keccak256("ERC3156FlashBorrower.onFlashLoan");

    // The loan in flight: set by `flashLoan`, read in Morpho's callback.
    address private receiver;
    address private initiator;

    error LoanInFlight();
    error NotMorpho();
    error NoLoan();
    error CallbackFailed();
    error TokenCallFailed();

    function maxFlashLoan(address token) external view returns (uint256) {
        return IERC20Calls(token).balanceOf(MORPHO);
    }

    function flashFee(address, uint256) external pure returns (uint256) {
        return 0;
    }

    function flashLoan(address borrower, address token, uint256 amount, bytes calldata data) external returns (bool) {
        if (receiver != address(0)) revert LoanInFlight();
        receiver = borrower;
        initiator = msg.sender;
        IMorphoFlash(MORPHO).flashLoan(token, amount, abi.encode(token, data));
        receiver = address(0);
        initiator = address(0);
        return true;
    }

    function onMorphoFlashLoan(uint256 assets, bytes calldata context) external {
        if (msg.sender != MORPHO) revert NotMorpho();
        address r = receiver;
        if (r == address(0)) revert NoLoan();
        (address token, bytes memory data) = abi.decode(context, (address, bytes));
        _call(token, abi.encodeCall(IERC20Calls.transfer, (r, assets)));
        if (IERC3156Borrower(r).onFlashLoan(initiator, token, assets, 0, data) != CALLBACK_SUCCESS) {
            revert CallbackFailed();
        }
        _call(token, abi.encodeCall(IERC20Calls.transferFrom, (r, address(this), assets)));
        // Morpho pulls the assets back once this returns.
        _call(token, abi.encodeCall(IERC20Calls.approve, (MORPHO, assets)));
    }

    /// A token call that must succeed and, when it returns data, return true.
    function _call(address token, bytes memory data) private {
        (bool ok, bytes memory ret) = token.call(data);
        if (!ok || (ret.length != 0 && (ret.length < 32 || !abi.decode(ret, (bool))))) {
            revert TokenCallFailed();
        }
    }
}
```

- [ ] **Step 5: Run the tests.** Run: `forge test --match-contract MorphoFlashLenderTest -vv`. Expected: 8 passed.
- [ ] **Step 6: The whole suite and the size gate.** Run `forge test 2>&1 | tail -3` (all suites pass) and `bash scripts/ci/check_executor_size.sh` (ok: the lender is a deployable contract under EIP-170).
- [ ] **Step 7: Mutants,** each run with `forge test --match-contract MorphoFlashLenderTest`. Each must fail a test:
  1. the `LoanInFlight` check deleted;
  2. `msg.sender != MORPHO` → `false`;
  3. `r == address(0)` → `false`;
  4. `!= CALLBACK_SUCCESS` → `== bytes32(0)`;
  5. the `transferFrom` pull deleted;
  6. the `approve` deleted;
  7. `_call`'s return check (`!abi.decode(ret, (bool))`) → `false` (killed by the false-returning token);
  8. `receiver = address(0)` after the loan deleted.

---

### Task 2: The proof on Base (simulation through the real executor)

**Files:**
- Modify: `crates/apex-runtime/src/armed/run.rs`. Extract `pub async fn book_after(...)` from `quote_at_fire`.
- Create: `crates/apex-runtime/tests/morpho_lender_proof.rs` (`#[ignore]`: network, and run in the main tree)

**Interfaces:**
- Consumes:
  - Task 1's artifact `out/MorphoFlashLender.sol/MorphoFlashLender.json`;
  - `apex_runtime::armed::run::{get_logs, replay_calls}`;
  - `apex_runtime::live::calls::encode_step`;
  - `apex_exec::{encode::PlanEncoder, commitment::{plan_commitment, Loan, LoanProvider}, sign::SignedPlan, call::ExecutorCall}`;
  - `apex_runtime::shadow::universe`.
- Produces:
  - `pub async fn book_after(reads: &ChainReads, specs: &[PoolSpec], pools: &[Address], block: u64, tx_index: u64, logs: &[RawLog]) -> Result<BTreeMap<Address, Arc<PoolSnapshot>>, String>`;
  - the measured `extra_gas`, printed as `PROOF extra_gas=<n>`, which Task 3's `MORPHO_EXTRA_GAS` takes.

- [ ] **Step 1: Extract `book_after`.** In `run.rs`, move `quote_at_fire`'s body from `let mine…` through `book.apply_writes(&writes);` into:

```rust
/// The book of `pools` as it stood after transaction `tx_index` of `block`: the
/// pools at `block`'s parent, then that block's writes to them through that
/// transaction. A Mint or Burn among them refuses, because the book would need
/// a reload to apply it.
pub async fn book_after(
    reads: &ChainReads,
    specs: &[PoolSpec],
    pools: &[Address],
    block: u64,
    tx_index: u64,
    logs: &[RawLog],
) -> Result<BTreeMap<Address, Arc<crate::live::book::PoolSnapshot>>, String> {
    let mine: Vec<PoolSpec> = specs.iter().filter(|s| pools.contains(&s.pool)).cloned().collect();
    let (book, unloaded) = PoolBook::load(reads, &mine, block - 1).await.map_err(|e| e.to_string())?;
    if !unloaded.is_empty() {
        return Err("a pool did not load at the block's parent".into());
    }
    let mut writes = Vec::new();
    for l in logs.iter().filter(|l| {
        !l.removed && l.block_number == Some(block) && pools.contains(&l.address) && l.transaction_index.is_some_and(|i| i <= tx_index)
    }) {
        let at = (block, l.log_index.unwrap_or(0));
        match l.topics.first() {
            Some(t) if *t == SWAP || *t == PANCAKE_SWAP => {
                let s = decode_swap(l).ok_or("an undecodable swap")?;
                writes.push(StateWrite::Swap(SwapWrite { pool: s.pool, sqrt_price_x96: s.sqrt_price_x96, liquidity: s.liquidity, tick: s.tick, at }));
            }
            Some(t) if *t == SYNC => {
                let s = decode_sync(l).ok_or("an undecodable sync")?;
                writes.push(StateWrite::Sync(SyncWrite { pool: s.pool, reserve0: s.reserve0, reserve1: s.reserve1, at }));
            }
            Some(t) if *t == MINT || *t == BURN => return Err("a mint or burn before the trigger in its block".into()),
            _ => {}
        }
    }
    book.apply_writes(&writes);
    Ok(book.snapshot())
}
```

  `quote_at_fire` becomes `let pools = …; let snap = book_after(reads, specs, &pools, fire.block, fire.tx_index, logs).await?;` followed by its existing `frontier::build` onward.
  - Run `cargo test -j 3 --offline -p apex-runtime --test armed_run --test armed_eval`: everything passes, so behaviour is unchanged.
  - The error text "a mint or burn before the trigger in its block" is unchanged; the evaluator's verdicts keep it.

- [ ] **Step 2: Write the proof.** `crates/apex-runtime/tests/morpho_lender_proof.rs`:

```rust
//! R27's proof, on Base, through the deployed executor: an ERC-3156 loan from
//! `MorphoFlashLender` settles correctly, and a real cycle above Balancer's
//! 19 WETH settles to its exact quote. The lender's code is injected by state
//! override unless `APEX_MORPHO_LENDER` names a deployed one.
//!
//! cargo build of the contract first: `forge build`. Then, in the main tree:
//! APEX_SECRET_BLOCKPI_KEY=… APEX_SECRET_TRADER_KEY=0x…01 \
//!   cargo test -j 3 --offline -p apex-runtime --test morpho_lender_proof -- --ignored --nocapture

use alloy_primitives::{address, keccak256, Address, U256};
use apex_chain::rpc::failover::{FailoverSettings, FailoverTransport};
use apex_chain::rpc::RpcTransport;
use apex_config::Env;
use apex_exec::call::ExecutorCall;
use apex_exec::commitment::{plan_commitment, Loan, LoanProvider};
use apex_exec::encode::PlanEncoder;
use apex_exec::sign::SignedPlan;
use apex_runtime::armed::run::{book_after, get_logs, replay_calls};
use apex_runtime::live::calls::encode_step;
use apex_runtime::live::frontier::{self, BALANCER_VAULT, WETH};
use apex_runtime::live::inventory::Venue;
use apex_runtime::live::pricing::LiveCycle;
use apex_runtime::live::reads::ChainReads;
use apex_runtime::shadow::config::ShadowConfig;
use apex_types::compat::{u256_to_alloy, u256_to_ethers};
use apex_types::ids::ChainId;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

const EXECUTOR: Address = address!("8940B565D050974b2b589B70De43bCb753b1F93B");
const TRADER: Address = address!("CB436Ba3acb945b3fc8EE6345857262584356595");
const MORPHO: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
/// Where the lender's code is injected when it is not yet deployed.
const INJECTED: Address = address!("000000000000000000000000000000000000beef");
/// The 21:00 UTC spike on 2026-10-12: ticket 51's trigger, route WETH -> USDC
/// on Uniswap's 0.3% pool -> WETH on its 0.05% pool (expected $32 at 17.88 WETH).
const BLOCK: u64 = 52_526_060;
const TRIGGER: &str = "0xd67a03d51f84c001b5e7b71f2c4b102c20d165a5991a1e8ed4b285f8118be84e";
const ROUTE: &str = "0xb257fc492c3711d488ec5032d116214b87590c08fa8f08ac769d32380694e2dc";
const POOLS: [Address; 2] =
    [address!("6c561b446416e1a00e8e93e221854d6ea4171372"), address!("d0b53d9277642d899df5c87a3966a349a798f224")];

fn hexq(v: &Value) -> u64 {
    u64::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
}

fn weth_slot(holder: Address) -> String {
    let mut k = [0u8; 64];
    k[12..32].copy_from_slice(holder.as_slice());
    k[63] = 3;
    format!("{:#x}", keccak256(k))
}

fn balance_call(holder: Address) -> Value {
    let mut data = vec![0x70, 0xa0, 0x82, 0x31];
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(holder.as_slice());
    json!({"from": format!("{TRADER:#x}"), "to": format!("{WETH:#x}"), "data": format!("0x{}", alloy_primitives::hex::encode(data))})
}

fn call_data(loan: Loan, steps: Vec<apex_exec::commitment::Step>, deadline: u64) -> String {
    let plan = PlanEncoder::build(loan, steps, 0, U256::from(1), 8453, deadline);
    let declared = plan_commitment(&plan, 8453, EXECUTOR);
    let call = ExecutorCall::from_checked(SignedPlan::check(plan, declared, 8453, EXECUTOR).unwrap());
    format!("0x{}", alloy_primitives::hex::encode(call.data()))
}

fn lender_code() -> String {
    let path = format!("{}/../../out/MorphoFlashLender.sol/MorphoFlashLender.json", env!("CARGO_MANIFEST_DIR"));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("forge build first")).unwrap();
    v["deployedBytecode"]["object"].as_str().unwrap().to_string()
}

/// Simulate `calls` at the state after `parent`, under `overrides`; the results.
async fn simulate(rpc: &dyn RpcTransport, parent: u64, block: &Value, overrides: Value, calls: Vec<Value>) -> Vec<Value> {
    let params = json!([{
        "blockStateCalls": [{
            "blockOverrides": { "number": format!("{:#x}", parent + 1), "time": block["timestamp"], "baseFeePerGas": block["baseFeePerGas"] },
            "stateOverrides": overrides,
            "calls": calls,
        }],
        "validation": false,
    }, format!("{parent:#x}")]);
    rpc.call("eth_simulateV1", params).await.unwrap()[0]["calls"].as_array().unwrap().clone()
}

fn gross_of(result: &Value) -> Option<U256> {
    let topic = format!("{:#x}", keccak256(b"ProfitRealised(address,uint256,uint256)"));
    result["logs"].as_array()?.iter().find(|l| l["topics"][0].as_str() == Some(&topic)).map(|l| {
        U256::from_str_radix(&l["data"].as_str().unwrap()[2..66], 16).unwrap()
    })
}

#[tokio::test]
#[ignore = "network: Base through BlockPI; run with --ignored in the main tree"]
async fn a_morpho_loan_settles_through_the_deployed_executor() {
    let path = format!("{}/../../ops/shadow.base.yaml", env!("CARGO_MANIFEST_DIR"));
    let config = ShadowConfig::load(&path, &Env::secrets_only()).unwrap();
    let urls: Vec<String> = config.rpc_http.iter().map(|u| u.expose().clone()).collect();
    let rpc: Arc<dyn RpcTransport> = Arc::new(FailoverTransport::connect(&urls, 8453, FailoverSettings::default()).await.unwrap());
    let reads = ChainReads::new(Arc::clone(&rpc));
    let (lender, overrides) = match std::env::var("APEX_MORPHO_LENDER") {
        Ok(a) => (a.parse::<Address>().unwrap(), json!({})),
        Err(_) => (INJECTED, json!({ format!("{INJECTED:#x}"): { "code": lender_code() } })),
    };
    let ten = format!("{:#066x}", U256::from(10u64) * U256::from(10u64).pow(U256::from(18)));
    let one = U256::from(10u64).pow(U256::from(18));

    // 1. A 1 WETH loan with no steps, the executor holding 10 WETH: zero profit,
    // so it must revert, as Balancer's does and Aave's does not.
    let latest = rpc.call("eth_getBlockByNumber", json!(["latest", false])).await.unwrap();
    let now = hexq(&latest["timestamp"]);
    let parent = hexq(&latest["number"]) - 1;
    let mut rich = overrides.clone();
    rich[format!("{WETH:#x}")] = json!({ "stateDiff": { weth_slot(EXECUTOR): ten } });
    let empty = |provider, addr| call_data(Loan { token: WETH, amount: one, provider, provider_addr: addr }, vec![], now + 120);
    let ours = |data: String| json!({"from": format!("{TRADER:#x}"), "to": format!("{EXECUTOR:#x}"), "data": data, "gas": "0x2dc6c0"});
    let r = simulate(&*rpc, parent, &latest, rich.clone(), vec![ours(empty(LoanProvider::Erc3156, lender))]).await;
    assert_eq!(r[0]["status"], "0x0", "an empty Morpho loan must revert for zero profit: {}", r[0]);
    let r = simulate(&*rpc, parent, &latest, rich.clone(), vec![ours(empty(LoanProvider::Balancer, BALANCER_VAULT))]).await;
    assert_eq!(r[0]["status"], "0x0", "the Balancer control reverts too");
    println!("PROOF 1 ok: an empty ERC-3156 loan reverts for zero profit");

    // 2. Ticket 51's cycle after its trigger, sized above Balancer's holding.
    let receipt = rpc.call("eth_getTransactionReceipt", json!([TRIGGER])).await.unwrap();
    let tx_index = hexq(&receipt["transactionIndex"]);
    let block = rpc.call("eth_getBlockByNumber", json!([format!("{BLOCK:#x}"), true])).await.unwrap();
    std::env::set_current_dir(format!("{}/../..", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let venues = [Venue::UniswapV3, Venue::Slipstream, Venue::PancakeV3, Venue::SlipstreamV3, Venue::AerodromeV2];
    let specs = apex_runtime::shadow::universe(&config.inventory, &venues, config.universe.filter()).unwrap();
    let pools: BTreeSet<Address> = POOLS.into_iter().collect();
    let logs = get_logs(&*rpc, &pools, BLOCK, BLOCK).await.unwrap();
    let snap = book_after(&reads, &specs, &POOLS, BLOCK, tx_index, &logs).await.unwrap();
    let (_, cycles) = frontier::build(ChainId::BASE, WETH, &snap);
    let cycle = cycles.values().find(|c| format!("{:#x}", c.commitment.route_hash) == ROUTE).expect("ticket 51's route");
    let live = LiveCycle::new(cycle, &snap).unwrap();
    let size = U256::from(20u64) * one;
    let [mid, out] = live.hop_outputs(u256_to_ethers(size)).expect("prices at 20 WETH").map(u256_to_alloy);
    assert!(out > size, "20 WETH pays after the trigger: {out} for {size}");
    let deadline = hexq(&block["timestamp"]) + 120;
    let steps = |s: U256, m: U256| {
        vec![
            encode_step(&cycle.legs[0], &snap[&cycle.legs[0].pool], s, m, EXECUTOR, deadline).unwrap(),
            encode_step(&cycle.legs[1], &snap[&cycle.legs[1].pool], m, s + U256::from(1), EXECUTOR, deadline).unwrap(),
        ]
    };
    let data = call_data(Loan { token: WETH, amount: size, provider: LoanProvider::Erc3156, provider_addr: lender }, steps(size, mid), deadline);
    let mut calls = replay_calls(&block, tx_index).unwrap();
    let before = calls.len();
    calls.push(balance_call(MORPHO));
    calls.push(ours(data));
    calls.push(balance_call(lender));
    calls.push(balance_call(MORPHO));
    let r = simulate(&*rpc, BLOCK - 1, &block, overrides.clone(), calls).await;
    let (morpho_before, settled, lender_after, morpho_after) = (&r[before], &r[before + 1], &r[before + 2], &r[before + 3]);
    assert_eq!(settled["status"], "0x1", "the 20 WETH Morpho-funded cycle settles: {settled}");
    assert_eq!(gross_of(settled), Some(out - size), "gross is the exact quote, to the wei");
    assert_eq!(U256::from_str_radix(&lender_after["returnData"].as_str().unwrap()[2..], 16).unwrap(), U256::ZERO, "the lender holds nothing");
    assert_eq!(morpho_before["returnData"], morpho_after["returnData"], "Morpho is repaid in full");
    let morpho_gas = hexq(&settled["gasUsed"]);
    println!("PROOF 2 ok: 20 WETH from Morpho, gross {} wei = quote, gas {morpho_gas}", out - size);

    // 3. The same cycle at 15 WETH from each lender: the gas Morpho adds.
    let small = U256::from(15u64) * one;
    let [mid15, out15] = live.hop_outputs(u256_to_ethers(small)).unwrap().map(u256_to_alloy);
    assert!(out15 > small);
    let mut gas = Vec::new();
    for (provider, addr) in [(LoanProvider::Balancer, BALANCER_VAULT), (LoanProvider::Erc3156, lender)] {
        let mut calls = replay_calls(&block, tx_index).unwrap();
        calls.push(ours(call_data(Loan { token: WETH, amount: small, provider, provider_addr: addr }, steps(small, mid15), deadline)));
        let r = simulate(&*rpc, BLOCK - 1, &block, overrides.clone(), calls).await;
        let last = r.last().unwrap();
        assert_eq!(last["status"], "0x1", "{provider:?} at 15 WETH: {last}");
        assert_eq!(gross_of(last), Some(out15 - small));
        gas.push(hexq(&last["gasUsed"]));
    }
    println!("PROOF 3 ok: 15 WETH, Balancer gas {}, Morpho gas {}, extra_gas={}", gas[0], gas[1], gas[1].saturating_sub(gas[0]));
}
```

- [ ] **Step 3: Run the proof.** Run `forge build`, then, in the main tree, with the key from `.env` exported under `APEX_SECRET_BLOCKPI_KEY` (as `$SCRATCH/smoke/eval.sh` does, never printed) and `APEX_SECRET_TRADER_KEY` set to the dummy `0x000…01`:
  `cargo test -j 3 --offline -p apex-runtime --test morpho_lender_proof -- --ignored --nocapture`
  Expected: `PROOF 1 ok`, `PROOF 2 ok`, and `PROOF 3 ok … extra_gas=<n>`.
  - **If any assertion fails, stop.** Report the failure; R27 does not continue (spec §2).
  - If `20 WETH pays after the trigger` fails, the state after ticket 51's trigger does not pay at 20 WETH. Rerun with the size changed to the largest paying size above 19 WETH. Find it with `CostedCycle::new(live.clone(), RouteCosts { other_wei: 0, wei_per_gas: 0 }, gas::MEASURED)` and `apex_math::finite_size::best_size`.
- [ ] **Step 4: Record** the three `PROOF` lines, with `extra_gas`, in `PLAN.md` (Task 8) and in Task 3's constant.

---

### Task 3: `Lenders` and pricing past Balancer's holding

**Files:**
- Create: `crates/apex-runtime/src/live/lenders.rs`, `crates/apex-runtime/tests/live_lenders.rs`
- Modify:
  - `crates/apex-runtime/src/live/mod.rs` (add `pub mod lenders;`);
  - `crates/apex-runtime/src/live/pricing.rs` (`CostedCycle.cap` becomes `lenders`; `LenderHolding` carries `Lenders`);
  - `crates/apex-runtime/tests/live_pricing.rs` (the two R21 lender tests, plus one new).

**Interfaces:**
- Consumes: Task 2's measured `extra_gas`.
- Produces:
  - `pub const MORPHO_FLASH: FlashProviderId`, `pub const MORPHO_BLUE: Address`, `pub const MORPHO_EXTRA_GAS: u64`;
  - `pub struct LenderChoice { pub id: FlashProviderId, pub provider: LoanProvider, pub address: Address, pub extra_gas: u64 }`, with `pub const BALANCER: LenderChoice` and `pub const fn morpho(lender: Address) -> LenderChoice`;
  - `pub struct Lenders { pub balancer: U256, pub morpho: Option<(Address, U256)> }` (ethers `U256`), with `balancer_only(U256)`, `cap(&self) -> U256`, `for_size(&self, U256) -> Option<LenderChoice>` and `extra_gas(&self, U256) -> u64`;
  - `pub fn loan(choice: &LenderChoice, token: Address, amount: alloy U256) -> Result<Loan, String>`;
  - `CostedCycle::lent(self, Option<Lenders>) -> Self` (replaces `capped`);
  - `pub type LenderHolding = watch::Receiver<Option<Lenders>>`.

- [ ] **Step 1: Write the failing tests.** `crates/apex-runtime/tests/live_lenders.rs`:

```rust
//! R27: which lender a size borrows from.

use alloy_primitives::{address, Address};
use apex_exec::commitment::LoanProvider;
use apex_runtime::live::frontier::{BALANCER_FLASH, BALANCER_VAULT, WETH};
use apex_runtime::live::lenders::{loan, morpho, Lenders, BALANCER, MORPHO_EXTRA_GAS, MORPHO_FLASH};
use ethers_core::types::U256;

const LENDER: Address = address!("00000000000000000000000000000000000000ee");

fn weth(n: u64) -> U256 {
    U256::from(n) * U256::exp10(18)
}

#[test]
fn a_size_borrows_from_balancer_while_it_covers_it_then_from_morpho() {
    let both = Lenders { balancer: weth(19), morpho: Some((LENDER, weth(82_679))) };
    assert_eq!(both.cap(), weth(82_679));
    assert_eq!(both.for_size(weth(1)), Some(BALANCER));
    assert_eq!(both.for_size(weth(19)), Some(BALANCER), "the whole holding is lendable");
    assert_eq!(both.for_size(weth(19) + 1), Some(morpho(LENDER)));
    assert_eq!(both.for_size(weth(82_679) + 1), None, "past both");
    assert_eq!((both.extra_gas(weth(19)), both.extra_gas(weth(20))), (0, MORPHO_EXTRA_GAS));
    assert_eq!((morpho(LENDER).id, morpho(LENDER).provider), (MORPHO_FLASH, LoanProvider::Erc3156));
    assert_eq!((BALANCER.id, BALANCER.provider, BALANCER.address), (BALANCER_FLASH, LoanProvider::Balancer, BALANCER_VAULT));

    let alone = Lenders::balancer_only(weth(19));
    assert_eq!((alone.cap(), alone.for_size(weth(20))), (weth(19), None));
    // Morpho holding less than Balancer still caps at the larger.
    let thin = Lenders { balancer: weth(19), morpho: Some((LENDER, weth(5))) };
    assert_eq!((thin.cap(), thin.for_size(weth(10))), (weth(19), Some(BALANCER)));
}

/// The deployed executor counts Aave's later pull as profit (simulated
/// 2026-10-13): no Aave loan is ever built.
#[test]
fn a_loan_names_its_lender_and_never_aave() {
    let amount = alloy_primitives::U256::from(5u64);
    let b = loan(&BALANCER, WETH, amount).unwrap();
    assert_eq!((b.token, b.amount, b.provider, b.provider_addr), (WETH, amount, LoanProvider::Balancer, BALANCER_VAULT));
    let m = loan(&morpho(LENDER), WETH, amount).unwrap();
    assert_eq!((m.provider, m.provider_addr), (LoanProvider::Erc3156, LENDER));
    let aave = apex_runtime::live::lenders::LenderChoice { provider: LoanProvider::Aave, ..BALANCER };
    assert!(loan(&aave, WETH, amount).unwrap_err().contains("cannot settle"));
}
```

  In `live_pricing.rs`, change the two R21 tests' channels to `tokio::sync::watch::channel(Some(Lenders::balancer_only(cap)))` and `watch::channel(None::<Lenders>)`, and add:

```rust
/// R27: past Balancer's holding a route sizes on into Morpho's, and each size
/// above it pays the Morpho lender's extra gas.
#[test]
fn a_route_sizes_past_balancer_into_morpho_at_its_extra_gas() {
    use apex_runtime::live::lenders::{Lenders, MORPHO_EXTRA_GAS};
    use apex_math::finite_size::SizedRoute;
    let b = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500, L), pool(SLIP, Venue::Slipstream, -197_300, 80, L)]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let free = LivePricer::new(b.clone(), cycles.clone(), flat(3_000_000_000_000), gas::MEASURED);
    let (id, unbounded) = cycles
        .keys()
        .find_map(|id| free.best_size(*id, &fp(), budget()).ok().map(|s| (*id, s.amount_in)))
        .expect("one direction pays");
    let balancer = unbounded / 4;
    let lenders = Lenders { balancer, morpho: Some((Address::repeat_byte(0xee), unbounded * 2)) };
    let (_tx, rx) = tokio::sync::watch::channel(Some(lenders));
    let pricer = LivePricer::new(b.clone(), cycles.clone(), flat(3_000_000_000_000), gas::MEASURED).with_lender(rx);
    let sized = pricer.best_size(id, &fp(), budget()).expect("pays");
    assert!(sized.amount_in > balancer, "sized past Balancer's {balancer}: {}", sized.amount_in);

    let costs = RouteCosts { other_wei: 0, wei_per_gas: 1 };
    let live = || LiveCycle::new(&cycles[&id], &b.snapshot()).unwrap();
    let above = balancer + 1;
    let with = CostedCycle::new(live(), costs, gas::MEASURED).lent(Some(lenders)).priced(above).unwrap().cost;
    let without = CostedCycle::new(live(), costs, gas::MEASURED).lent(Some(Lenders::balancer_only(unbounded * 2))).priced(above).unwrap().cost;
    assert_eq!(with - without, U256::from(MORPHO_EXTRA_GAS), "one unit a gas: the difference is the extra gas");
    let at = CostedCycle::new(live(), costs, gas::MEASURED).lent(Some(lenders)).priced(balancer).unwrap().cost;
    let at_free = CostedCycle::new(live(), costs, gas::MEASURED).lent(Some(Lenders::balancer_only(unbounded * 2))).priced(balancer).unwrap().cost;
    assert_eq!(at, at_free, "Balancer's whole holding borrows at no extra gas");
}
```

  The test file's existing imports gain `apex_runtime::live::pricing::CostedCycle` and `apex_runtime::econ::RouteCosts`, if not already imported.

- [ ] **Step 2: Run them to see them fail.** Run: `cargo test -j 3 --offline -p apex-runtime --test live_lenders --test live_pricing`. Expected: compile errors (`lenders` module, `Lenders`, `lent`).

- [ ] **Step 3: Write `lenders.rs`:**

```rust
//! The flash lenders a cycle borrows from (Task 8.5 R27).
//!
//! - Balancer V2's vault: free, ~19 WETH on Base.
//! - Morpho Blue through `MorphoFlashLender` (ERC-3156): free, ~82,679 WETH
//!   (2026-10-13).
//!
//! A size borrows from Balancer while Balancer covers it (no wrapper hops),
//! and from Morpho above. Never from Aave: the deployed executor counts Aave's
//! later pull as profit (simulated 2026-10-13).

use crate::live::frontier::{BALANCER_FLASH, BALANCER_VAULT};
use alloy_primitives::{address, Address};
use apex_exec::commitment::{Loan, LoanProvider};
use apex_types::ids::FlashProviderId;
use ethers_core::types::U256;

/// The Morpho lender's loans: the next id after Balancer's.
pub const MORPHO_FLASH: FlashProviderId = FlashProviderId(2);

/// Morpho Blue on Base, whose holding is what the Morpho lender can lend.
pub const MORPHO_BLUE: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");

/// What a loan through `MorphoFlashLender` costs over Balancer's.
/// R27 Task 2 measured it on ticket 51's cycle at 15 WETH (block 52,526,060):
/// the gas difference, rounded up to the next 1,000.
pub const MORPHO_EXTRA_GAS: u64 = 0; // Task 2's `extra_gas`, rounded up to the next 1,000.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LenderChoice {
    pub id: FlashProviderId,
    pub provider: LoanProvider,
    /// The address the plan's loan names.
    pub address: Address,
    /// The gas this lender adds over Balancer's.
    pub extra_gas: u64,
}

pub const BALANCER: LenderChoice =
    LenderChoice { id: BALANCER_FLASH, provider: LoanProvider::Balancer, address: BALANCER_VAULT, extra_gas: 0 };

/// The Morpho lender deployed at `lender`.
pub const fn morpho(lender: Address) -> LenderChoice {
    LenderChoice { id: MORPHO_FLASH, provider: LoanProvider::Erc3156, address: lender, extra_gas: MORPHO_EXTRA_GAS }
}

/// What each lender holds of the loan token, as last read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lenders {
    pub balancer: U256,
    /// The Morpho lender's address and Morpho Blue's holding, when configured.
    pub morpho: Option<(Address, U256)>,
}

impl Lenders {
    pub const fn balancer_only(holding: U256) -> Self {
        Self { balancer: holding, morpho: None }
    }

    /// The most any size may borrow.
    pub fn cap(&self) -> U256 {
        self.morpho.map_or(self.balancer, |(_, m)| m.max(self.balancer))
    }

    /// The lender `size` borrows from: Balancer while it covers it, then Morpho.
    pub fn for_size(&self, size: U256) -> Option<LenderChoice> {
        if size <= self.balancer {
            return Some(BALANCER);
        }
        self.morpho.filter(|(_, m)| size <= *m).map(|(a, _)| morpho(a))
    }

    /// The gas `size`'s loan adds over Balancer's.
    pub fn extra_gas(&self, size: U256) -> u64 {
        self.for_size(size).map_or(0, |c| c.extra_gas)
    }
}

/// The executor's loan of `amount` of `token` from `choice`.
pub fn loan(choice: &LenderChoice, token: Address, amount: alloy_primitives::U256) -> Result<Loan, String> {
    match choice.provider {
        LoanProvider::Balancer | LoanProvider::Erc3156 => {
            Ok(Loan { token, amount, provider: choice.provider, provider_addr: choice.address })
        }
        other => Err(format!("the deployed executor cannot settle a {other:?} loan (R27)")),
    }
}
```

  Then set `MORPHO_EXTRA_GAS` to Task 2's `extra_gas` rounded up to the next 1,000, and replace the doc line's trailing comment with the measured figures (Balancer gas, Morpho gas).

- [ ] **Step 4: Change pricing.** In `pricing.rs`:
  - `pub type LenderHolding = tokio::sync::watch::Receiver<Option<crate::live::lenders::Lenders>>;`
  - `CostedCycle` field `cap: Option<U256>` → `lenders: Option<crate::live::lenders::Lenders>`; `new` sets `lenders: None`.
  - Replace `capped` with:

```rust
    /// Bound every size at what the lenders hold, as well as at the paying
    /// pool's holding, and charge a size its lender's extra gas (R21, R27).
    #[must_use]
    pub const fn lent(mut self, lenders: Option<crate::live::lenders::Lenders>) -> Self {
        self.lenders = lenders;
        self
    }

    /// The settlement's gas at `amount_in`, its loan's extra gas included.
    fn gas_for(&self, hops: &[HopSteps], amount_in: U256) -> GasEstimate {
        let mut g = self.gas.estimate(hops);
        let extra = self.lenders.map_or(0, |l| l.extra_gas(amount_in));
        g.expected = GasUsed(g.expected.0 + extra);
        g.ceiling = GasUsed(g.ceiling.0 + extra);
        g
    }
```

  - `max_input`: `self.lenders.map_or(held, |l| held.min(l.cap()))`.
  - `priced`: `cost: self.cost_of(self.gas_for(&q.hops, amount_in))`.
  - `gas_at`: `self.cycle.quote(amount_in).map(|q| self.gas_for(&q.hops, amount_in))`.
  - `fixed_cost` stays at the bare estimate (the least a size costs).
  - `LivePricer::live`: `let lenders = self.lender.as_ref().map(|l| (*l.borrow()).unwrap_or(Lenders::balancer_only(U256::zero())));` then `.lent(lenders)`. An unread holding still sizes nothing.
  - Update `with_lender`'s doc: "Size every cycle within what the lenders hold as `lender` reports it (R21, R27)".
  - Add `use apex_types::cost::GasUsed;` and `use crate::live::lenders::Lenders;` where missing.
  - In `armed/threshold.rs` (Task 5 finishes it), the one `.capped(px.cap)` call temporarily becomes `.lent(px.cap.map(Lenders::balancer_only))` so the crate compiles.

- [ ] **Step 5: Run the tests.** Run: `cargo test -j 3 --offline -p apex-runtime --test live_lenders --test live_pricing --test armed_threshold --test armed_engine`. Expected: all pass.

---

### Task 4: The reactive lane borrows from the lender that covers the size

**Files:**
- Modify:
  - `crates/apex-runtime/src/econ.rs` (`flash: Vec<FlashTerms>`; `with_flash` appends; `flash_quote` falls back);
  - `crates/apex-runtime/src/live/calls.rs` (`with_morpho`; the loan from `k.flash_source`);
  - `crates/apex-runtime/src/live/reader.rs` (`ReaderConfig.morpho`; `ChainView.morpho_holding`; `flash_available` by lender).
- Test: `crates/apex-runtime/tests/live_econ.rs`, `live_calls.rs`, `live_reader.rs`

**Interfaces:**
- Consumes: Task 3's `lenders::{BALANCER, morpho, loan, MORPHO_FLASH, MORPHO_BLUE}`.
- Produces:
  - `LiveCallBuilder::with_morpho(self, lender: Address) -> Self`;
  - `ReaderConfig { …, morpho: Option<Address> }` (Morpho Blue's address when the Morpho lender is configured);
  - `ChainView { …, morpho_holding: u128 }`.

- [ ] **Step 1: Write the failing tests.** In `live_econ.rs`, at the end of `a_borrowing_route_is_quoted_on_its_lenders_terms` (before the `p.flash_source = None` lines), add:

```rust
    // R27: past its own lender's holding, a route borrows from another lender of
    // the same asset that covers it; with both covering, its own (Balancer).
    let short = size.get() - AlloyU256::from(1u64);
    let fallback = base().with_flash(terms(1, short)).with_flash(terms(2, AlloyU256::MAX));
    let q = fallback.assemble(&p, refinement()).unwrap().flash_source.unwrap();
    assert_eq!((q.provider, q.availability_probability), (FlashProviderId(2), 1.0));
    let both = base().with_flash(terms(1, AlloyU256::MAX)).with_flash(terms(2, AlloyU256::MAX));
    assert_eq!(both.assemble(&p, refinement()).unwrap().flash_source.unwrap().provider, FlashProviderId(1));
    let neither = base().with_flash(terms(1, short)).with_flash(terms(2, short));
    let q = neither.assemble(&p, refinement()).unwrap().flash_source.unwrap();
    assert_eq!((q.provider, q.availability_probability), (FlashProviderId(1), 0.0), "quoted on its own terms, unavailable");
```

  In `live_calls.rs`, replace `only_balancers_loan_is_wired` with:

```rust
/// R27: the commitment's lender is the plan's. Balancer's vault; the Morpho
/// lender, once it is configured, as an ERC-3156 loan from its address;
/// nothing else.
#[test]
fn the_plan_borrows_from_the_commitments_lender() {
    use apex_runtime::live::lenders::MORPHO_FLASH;
    let lender = Address::repeat_byte(0xee);
    let b = book(-197_350, -197_300);
    let (cycle, c) = priced(&b);
    let morpho_k = ExecutionCommitment { flash_source: MORPHO_FLASH, ..commitment(&c, 1) };
    let err = LiveCallBuilder::new(b.clone(), [cycle.clone()]).build(&c, &morpho_k).unwrap_err();
    assert!(matches!(&err, Decline::Uncommittable { detail } if detail.contains("Morpho")), "unconfigured: {err:?}");
    let call = LiveCallBuilder::new(b.clone(), [cycle.clone()]).with_morpho(lender).build(&c, &morpho_k).expect("builds");
    let loan = call.plan().loans[0];
    assert_eq!((loan.provider, loan.provider_addr, loan.amount), (LoanProvider::Erc3156, lender, c.input_amount.get()));
    for source in [FlashProviderId(0), FlashProviderId(3)] {
        let k = ExecutionCommitment { flash_source: source, ..commitment(&c, 1) };
        let err = LiveCallBuilder::new(b.clone(), [cycle.clone()]).with_morpho(lender).build(&c, &k).unwrap_err();
        assert!(matches!(err, Decline::Uncommittable { .. }), "{source:?}: {err:?}");
    }
}
```

  In `live_reader.rs`:
  - Teach the fake `Node`'s `eth_call` to answer Morpho Blue's holding when the request names it: if the request's `data` (lowercase hex) contains `bbbbbbbbbb9cc5e90e3b3af64bdaf62c37eeffcb`, answer `aggregate3_answer(U256::from(82_679u128 * 10u128.pow(18)))`, else the existing 29.08 WETH.
  - Add `morpho: None` to `config()`.
  - Add:

```rust
/// R27: with the Morpho lender configured, a fifth read takes Morpho Blue's
/// holding, and a ticket borrowing from it is checked against that.
#[tokio::test]
async fn a_ticket_borrowing_from_morpho_is_checked_against_morphos_holding() {
    use apex_runtime::live::lenders::{MORPHO_BLUE, MORPHO_FLASH};
    let n = node();
    let r = ChainReader::new(n.clone(), book(), ReaderConfig { morpho: Some(MORPHO_BLUE), ..config(Duration::from_secs(60)) });
    r.refresh(&head(101), 30_000_000).await.unwrap();
    assert_eq!(n.calls.swap(0, Ordering::SeqCst), 5, "Morpho's holding is a fifth read");
    let mut t = ticket();
    assert_eq!(r.read(&t).await.unwrap().flash_available, 29_082_963_936_604_904_861, "no lender named: Balancer's");
    t.flash_source = Some(apex_types::flash::FlashSourceQuote {
        provider: MORPHO_FLASH,
        asset: apex_types::ids::TokenId { chain: t.chain_id, address: WETH },
        amount: t.exact_input,
        premium: U256::ZERO,
        gas_overhead: 0,
        callback_constraints: apex_types::flash::CallbackConstraints { repay_by_transfer: true, reentrancy_permitted: false, max_callback_gas: u64::MAX },
        availability_probability: 1.0,
        state_dependencies: Vec::new(),
        reliability_score: 1.0,
    });
    assert_eq!(r.read(&t).await.unwrap().flash_available, 82_679 * 10u128.pow(18));
}
```

- [ ] **Step 2: Run them to see them fail.** Run: `cargo test -j 3 --offline -p apex-runtime --test live_econ --test live_calls --test live_reader`. Expected: compile errors (`with_morpho`, `morpho`, `morpho_holding`) and the econ assertion failing on provider 1.

- [ ] **Step 3: The economics.** In `econ.rs`:
  - the field becomes `flash: Vec<FlashTerms>`, and `new` sets `flash: Vec::new()`;
  - `with_flash` does `self.flash.push(terms); self` (no longer `const`), and its doc says "A lender a borrowing route may be quoted against; the first added is a route's own";
  - in `flash_quote`, replace the `let t = …` lookup with:

```rust
        let own = self.flash.iter().find(|t| t.provider == provider).ok_or_else(|| {
            Decline::Uncommittable { detail: format!("no terms for flash provider {}", provider.0) }
        })?;
        // R27: past its own lender's holding, another lender of the same asset
        // that covers the amount; failing every one, its own, quoted unavailable.
        let t = if amount <= own.available {
            own
        } else {
            self.flash.iter().find(|t| t.asset == own.asset && amount <= t.available).unwrap_or(own)
        };
```

  The quote then uses `t.provider` as `provider`. The existing `availability_probability` line reads `t.available`.

- [ ] **Step 4: The call builder.** In `calls.rs`:
  - `LiveCallBuilder` gains `morpho: Option<Address>`, and `new` sets `None`;
  - add the builder method:

```rust
    /// Build loans from the Morpho lender at `lender` as well as Balancer's (R27).
    #[must_use]
    pub fn with_morpho(mut self, lender: Address) -> Self {
        self.morpho = Some(lender);
        self
    }
```

  In `build`, replace the `if k.flash_source != BALANCER_FLASH { … }` block with:

```rust
        let choice = match k.flash_source {
            BALANCER_FLASH => crate::live::lenders::BALANCER,
            crate::live::lenders::MORPHO_FLASH => crate::live::lenders::morpho(
                self.morpho.ok_or_else(|| refuse("the Morpho lender is not configured"))?,
            ),
            other => return Err(refuse(format!("flash source {} is not wired", other.0))),
        };
```

  and the `let loan = Loan { … }` with `let loan = crate::live::lenders::loan(&choice, cycle.start, input).map_err(refuse)?;`. Remove the `BALANCER_VAULT` and `Loan`/`LoanProvider` imports if now unused.

- [ ] **Step 5: The reader.**
  - `ReaderConfig` gains `pub morpho: Option<Address>`, with the doc "Morpho Blue, when the Morpho lender is configured: its holding is that lender's".
  - `ChainView` gains `pub morpho_holding: u128` (0 when unconfigured).
  - In `refresh`, after the existing `tokio::join!`, when `c.morpho` is `Some(m)`, multicall `[(c.loan_token, abi::call_address(selector::BALANCE_OF, m))]` at `head.number` and decode it as `lender_holding` is decoded. It is a separate call, so the four-read test stays four.
  - In `read`: `flash_available: if t.flash_source.as_ref().is_some_and(|f| f.provider == crate::live::lenders::MORPHO_FLASH) { v.morpho_holding } else { v.lender_holding },`.
  - The shadow's `ReaderConfig` literal gets `morpho: None` until Task 6.

- [ ] **Step 6: Run the tests.** Run: `cargo test -j 3 --offline -p apex-runtime --tests 2>&1 | grep -E "test result|FAILED|panicked" | sort | uniq -c`. Expected: every result ok (the end-to-end, lifecycle and one-call tests build plans through the same builder).

---

### Task 5: The armed lane sizes past Balancer and names its lender

**Files:**
- Modify:
  - `crates/apex-runtime/src/armed/threshold.rs` (`Pricing.lenders` replaces `cap`);
  - `crates/apex-runtime/src/armed/plan.rs` (`armed_call` takes `&LenderChoice`);
  - `crates/apex-runtime/src/armed/engine.rs` (the choice per rung; `Arm.lender`);
  - `crates/apex-runtime/src/armed/lines.rs` (`Arm.lender`).
- Test: `crates/apex-runtime/tests/armed_engine.rs`, `armed_threshold.rs`, `armed_predicate.rs`, `armed_eval.rs` (its `arm()` literal), `armed_run.rs` (its JSON arm)

**Interfaces:**
- Consumes: Task 3's `Lenders`, `LenderChoice`, `loan`.
- Produces:
  - `Pricing { costs, gas, lenders: Option<Lenders>, margin_wei }`;
  - `armed_call(cycle, pools, size, mid, executor, chain_id, deadline, lender: &LenderChoice)`;
  - `Arm { …, #[serde(default = "balancer")] pub lender: String }`, holding `"balancer"` or `"morpho"`.

- [ ] **Step 1: Write the failing test.** In `armed_engine.rs` (whose `px()` becomes `lenders: Some(Lenders::balancer_only(U256::from(10u128.pow(21))))`), add:

```rust
/// R27: a rung sized past Balancer's holding borrows from the Morpho lender,
/// and its calldata names that lender; one within it borrows from Balancer.
#[test]
fn a_rung_past_balancers_holding_borrows_from_morpho() {
    use apex_runtime::live::lenders::Lenders;
    let lender = Address::repeat_byte(0xee);
    let (engine, snap) = setup(16);
    let small = U256::from(10u128.pow(17)); // 0.1 WETH
    let px = Pricing { lenders: Some(Lenders { balancer: small, morpho: Some((lender, U256::from(10u128.pow(21)))) }), ..px() };
    let arms: Vec<Arm> = engine
        .tick(&snap, 1_000, 5_000_000, &px, 1)
        .into_iter()
        .filter_map(|l| if let ArmedLine::Arm(a) = l { Some(*a) } else { None })
        .collect();
    let needle = format!("{lender:#x}").trim_start_matches("0x").to_string();
    assert!(arms.iter().any(|a| a.lender == "morpho"), "some rung sizes past 0.1 WETH");
    for a in &arms {
        let size = U256::from_dec_str(&a.size_wei).unwrap();
        assert_eq!(a.lender, if size <= small { "balancer" } else { "morpho" }, "arm {} at {size}", a.arm_id);
        assert_eq!(a.calldata.contains(&needle), a.lender == "morpho");
    }
}
```

  `armed_engine.rs` imports gain `apex_runtime::armed::lines::Arm` (if missing). In `armed_threshold.rs`, `px(margin)` becomes `lenders: Some(Lenders::balancer_only(U256::from(10u128.pow(21))))`.

- [ ] **Step 2: Run it to see it fail.** Run: `cargo test -j 3 --offline -p apex-runtime --test armed_engine`. Expected: compile errors (`lenders`, `lender`).

- [ ] **Step 3: Implement.**
  - **`threshold.rs`:**
    - `Pricing.cap: Option<U256>` → `pub lenders: Option<crate::live::lenders::Lenders>`, with the doc "What the lenders hold: the most any size may borrow, and from whom (R27)".
    - `best_at`: `.lent(px.lenders)` (replacing Task 3's temporary bridge).
  - **`plan.rs`:**
    - `armed_call(…, deadline: u64, lender: &crate::live::lenders::LenderChoice)`;
    - the loan becomes `crate::live::lenders::loan(lender, cycle.start, size)?`;
    - drop the `BALANCER_VAULT`, `Loan` and `LoanProvider` imports if unused.
  - **`lines.rs`:** `Arm` gains, after `sender_slot`:

```rust
    /// Who lends the size: "balancer" or "morpho" (R27). Lines before R27
    /// borrowed from Balancer.
    #[serde(default = "balancer")]
    pub lender: String,
```

    with `fn balancer() -> String { "balancer".into() }`.
  - **`engine.rs`:**
    - `calldata()` gains `px: &Pricing`. It chooses `let choice = px.lenders.and_then(|l| l.for_size(band.point.size)).unwrap_or(crate::live::lenders::BALANCER);` and passes `&choice` to `armed_call`. It returns `(calldata, hash, lender_name)`, where `lender_name` is `if choice.id == crate::live::lenders::MORPHO_FLASH { "morpho" } else { "balancer" }`.
    - `arm()` sets `lender` from it.
    - The renewal path passes `px` too, and keeps `live.arm.lender` unchanged: a renewal re-signs the same size.
    - The engine's `tick` already has `px`.
  - **Callers in tests:** `armed_predicate.rs`'s `armed_call(...)` calls get `&apex_runtime::live::lenders::BALANCER`. `armed_eval.rs`'s `arm()` literal gets `lender: "balancer".into()`. `armed_run.rs`'s JSON arm needs nothing (serde default).

- [ ] **Step 4: Run the armed tests.** Run: `cargo test -j 3 --offline -p apex-runtime --test armed_engine --test armed_threshold --test armed_predicate --test armed_eval --test armed_run`. Expected: all pass.

---

### Task 6: Configuration, boot verification and the shadow's wiring

**Files:**
- Modify:
  - `crates/apex-runtime/src/shadow/config.rs` (optional `lenders` section);
  - `crates/apex-runtime/src/shadow/mod.rs` (verify at boot, read, terms, reader, builder, head loop, armed pricing, report);
  - `scripts/shadow-status.sh` (a `lenders` line);
  - `ops/shadow.base.yaml` (a commented `lenders:` example until Task 8);
  - `ops/host/README.md` (§9: reading the lenders line).
- Test: `crates/apex-runtime/tests/shadow_parts.rs`

**Interfaces:**
- Consumes: Tasks 3–5.
- Produces:
  - `ShadowConfig { …, pub lenders: LendersConfig }`, where `pub struct LendersConfig { pub morpho: Option<MorphoLenderConfig> }` and `pub struct MorphoLenderConfig { pub address: Address, pub code_hash: B256 }`;
  - `pub fn lender_code_ok(found: B256, config: &MorphoLenderConfig) -> Result<(), String>`;
  - `Report.lenders: LendersReport { balancer_wei: String, morpho_wei: Option<String> }`.

- [ ] **Step 1: Write the failing tests.** In `shadow_parts.rs`:

```rust
/// R27: the Morpho lender is configured by address and the code it must run.
/// Absent, the shadow borrows from Balancer alone. A wrong code hash refuses.
#[test]
fn the_morpho_lender_is_named_with_its_code() {
    use apex_runtime::shadow::config::{lender_code_ok, MorphoLenderConfig};
    let var = "APEX_SECRET_SHADOW_TEST_LENDER";
    let plain = ShadowConfig::from_yaml_str(&yaml(var, "var/apex/shadow.journal"), &env(var, SECRET)).unwrap();
    assert_eq!(plain.lenders.morpho, None);
    let text = yaml(var, "var/apex/shadow.journal")
        + "lenders:\n  morpho:\n    address: \"0x00000000000000000000000000000000000000ee\"\n    code_hash: \"0x1111111111111111111111111111111111111111111111111111111111111111\"\n";
    let c = ShadowConfig::from_yaml_str(&text, &env(var, SECRET)).unwrap();
    let m = c.lenders.morpho.clone().unwrap();
    assert_eq!(m, MorphoLenderConfig { address: Address::repeat_byte(0xee), code_hash: B256::repeat_byte(0x11) });
    assert!(lender_code_ok(B256::repeat_byte(0x11), &m).is_ok());
    assert!(lender_code_ok(B256::repeat_byte(0x22), &m).unwrap_err().contains("code"));
    let typo = text.replace("code_hash", "codehash");
    assert!(ShadowConfig::from_yaml_str(&typo, &env(var, SECRET)).is_err(), "unknown fields refused");
}
```

  Adjust `B256`/`Address` imports in the test file as needed. The `yaml()` fixture ends with a top-level key, so appending a `lenders:` block parses.

- [ ] **Step 2: Run it to see it fail.** Run: `cargo test -j 3 --offline -p apex-runtime --test shadow_parts`. Expected: compile errors (`lenders`, `MorphoLenderConfig`, `lender_code_ok`).

- [ ] **Step 3: Config.** In `config.rs`:

```rust
/// The flash lenders beside Balancer's vault (R27).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LendersConfig {
    #[serde(default)]
    pub morpho: Option<MorphoLenderConfig>,
}

/// `MorphoFlashLender`: where it is deployed and the code it must run there.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MorphoLenderConfig {
    pub address: Address,
    /// keccak256 of its runtime code, read back from the chain after deploy.
    pub code_hash: B256,
}

/// Whether the code found at the Morpho lender's address is the code it must
/// run. Nothing is borrowed from an address whose code was not read.
pub fn lender_code_ok(found: B256, config: &MorphoLenderConfig) -> Result<(), String> {
    if found == config.code_hash {
        Ok(())
    } else {
        Err(format!("the Morpho lender at {:#x} runs code {found:#x}, not {:#x}", config.address, config.code_hash))
    }
}
```

  `Raw` gains `#[serde(default)] lenders: LendersConfig,`, `ShadowConfig` gains `pub lenders: LendersConfig,`, and `from_yaml_str` passes `lenders: raw.lenders`.

- [ ] **Step 4: The shadow.** In `shadow/mod.rs`:
  - **Boot,** after the Balancer holding is read:

```rust
    // R27: the Morpho lender, when configured. Its code is read and must be
    // the code it was deployed with; its holding is Morpho Blue's.
    let morpho = match &config.lenders.morpho {
        None => None,
        Some(m) => {
            let found = reads.code_hash(m.address, head).await.map_err(|e| boot_err("the Morpho lender's code", e))?;
            crate::shadow::config::lender_code_ok(found, m).map_err(ShadowError::Boot)?;
            let held = reads
                .multicall(&[(WETH, abi::call_address(selector::BALANCE_OF, crate::live::lenders::MORPHO_BLUE))], head)
                .await
                .map_err(|e| boot_err("Morpho's holding", e))?
                .first()
                .cloned()
                .flatten()
                .and_then(|d| abi::word_uint(&d, 0, 128))
                .ok_or_else(|| ShadowError::Boot("Morpho's holding did not decode".into()))?;
            Some((m.address, held))
        }
    };
```

  - **Lenders and terms:**
    - `let lenders = crate::live::lenders::Lenders { balancer: ethers_core::types::U256::from(holding), morpho: morpho.map(|(a, h)| (a, ethers_core::types::U256::from(h))) };`
    - The `lender` watch channel is created with `Some(lenders)` instead of `Some(U256::from(holding))`.
    - When `morpho` is `Some((_, h))`, a second terms value, `let morpho_terms = FlashTerms { provider: crate::live::lenders::MORPHO_FLASH, available: U256::from(h), ..terms };` (Balancer's `terms` supplies `asset` and `callback`), is added with `.with_flash(morpho_terms)` after `.with_flash(terms)`.
  - `ReaderConfig { …, morpho: morpho.map(|_| crate::live::lenders::MORPHO_BLUE) }`.
  - `LiveCallBuilder::new(...)`, then `if let Some((a, _)) = morpho { builder = builder.with_morpho(a) }`.
  - **Head loop** (where `held` is built today):

```rust
                let lenders = crate::live::lenders::Lenders {
                    balancer: ethers_core::types::U256::from(v.lender_holding),
                    morpho: self.config.lenders.morpho.as_ref().map(|m| (m.address, ethers_core::types::U256::from(v.morpho_holding))),
                };
                self.lender.send_if_modified(|now| {
                    let moved = *now != Some(lenders);
                    *now = Some(lenders);
                    moved
                });
```

    The economics' terms keep their boot `available`. The risk gate's availability is a pricing hint; last-mile check 9 reads the live view. Say so in a comment.
  - **Armed pricing:** `lenders: *self.lender.borrow()` replaces `cap: *self.lender.borrow()`.
  - **Report:** add `lenders: LendersReport { balancer_wei, morpho_wei }` from `*self.lender.borrow()`. In `shadow-status.sh`, print `lenders     balancer <x> WETH, morpho <y> WETH` (or `morpho not configured`) from `r["lenders"]`.
  - **`ops/shadow.base.yaml`,** at the end:

```yaml
# R27: MorphoFlashLender (Morpho Blue's free flash loans as ERC-3156). Set once
# deployed: its address and keccak256 of its runtime code read back from the
# chain. Absent, every loan is Balancer's.
# lenders:
#   morpho:
#     address: "0x…"
#     code_hash: "0x…"
```

  - **README §9:** a `lenders` bullet. It shows what each lender holds; arms past Balancer's holding are labelled `morpho` in `armed-*.jsonl`.

- [ ] **Step 5: Run the tests and the whole crate.** Run: `cargo test -j 3 --offline -p apex-runtime --test shadow_parts`, then `cargo test -j 3 --offline -p apex-runtime -p apex-chain -p apex-math 2>&1 | grep -E "test result|FAILED|panicked" | sort | uniq -c`. Expected: all pass.

---

### Task 7: The deploy script, its dry run, and the operator's broadcast

**Files:**
- Create: `script/DeployMorphoFlashLender.s.sol`

**Interfaces:**
- Consumes: Task 1's contract.
- Produces: the deployed address and its runtime code hash (Task 8's config).

- [ ] **Step 1: Write the script:**

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {Script, console2} from "forge-std/Script.sol";
import {MorphoFlashLender} from "../contracts/lenders/MorphoFlashLender.sol";

/// R27: deploy MorphoFlashLender on Base. No constructor arguments, no owner,
/// nothing to configure afterwards.
///
///   forge script script/DeployMorphoFlashLender.s.sol --rpc-url base                 # dry run
///   forge script script/DeployMorphoFlashLender.s.sol --rpc-url base --broadcast …   # the operator
contract DeployMorphoFlashLender is Script {
    function run() external returns (MorphoFlashLender lender) {
        require(block.chainid == 8453, "Base only");
        require(lender.MORPHO().code.length > 0, "no Morpho Blue at the constant");
        vm.startBroadcast();
        lender = new MorphoFlashLender();
        vm.stopBroadcast();
        console2.log("MorphoFlashLender", address(lender));
        console2.log("runtime code hash");
        console2.logBytes32(keccak256(address(lender).code));
    }
}
```

- [ ] **Step 2: Dry run.** Run `forge clean`, then `BLOCKPI_KEY=… forge script script/DeployMorphoFlashLender.s.sol --rpc-url base --sender <the operator's deployer address>`, with the key exported from `.env`, never printed. Expected: `MorphoFlashLender 0x…` and a code hash. Ignore the 21 "failed to parse" warnings, which are known noise. Also run `forge test --match-contract MorphoFlashLenderTest` once more.
- [ ] **Step 3: Hand over to the operator.** Give the operator the exact broadcast command (their keystore account or key from their environment, never on this machine's command line by me), the expected gas (~300k at ~0.005 gwei, under $0.01) and the dry-run address. **Wait for the operator to broadcast and report the deployed address.**
- [ ] **Step 4: Verify the deployment** (read-only):
  - `eth_getCode` at the address is non-empty;
  - its keccak256 equals the dry run's code hash and `keccak256(deployedBytecode)` of `out/…/MorphoFlashLender.json`;
  - `MORPHO()` returns Morpho Blue;
  - `maxFlashLoan(WETH)` equals `WETH.balanceOf(Morpho Blue)`.
  - Then run Task 2's proof against it with `APEX_MORPHO_LENDER=<address>` (no code injection). Expected: all three `PROOF` lines.

---

### Task 8: Gates, mutations, record, commit, deploy

- [ ] **Step 1: Gates.** `bash $SCRATCH/gates.sh`: everything green but the two known-red scripts. `forge test` includes the lender's suite. `check_executor_size.sh` passes.
- [ ] **Step 2: Rust mutants,** with `$SCRATCH/mutate26.py`'s runner extended by these entries. Each must be killed:
  1. `Lenders::for_size` `size <= self.balancer` → `<`, killed by `live_lenders`;
  2. `for_size`'s Morpho filter `size <= *m` → `true`, by `live_lenders`;
  3. `cap()`'s `.max(self.balancer)` deleted (Morpho's holding alone), by `live_lenders` (the thin case);
  4. `loan()`'s Aave refusal (`other => Err(..)` → `Ok(Loan{..})`), by `live_lenders`;
  5. `gas_for`'s extra (`+ extra` → `+ 0`), by `live_pricing`;
  6. `max_input`'s `l.cap()` → `l.balancer`, by `live_pricing`;
  7. `flash_quote`'s fallback (`amount <= t.available` in the `find` → `false`), by `live_econ`;
  8. the call builder's Morpho arm mapped to `BALANCER`, by `live_calls`;
  9. the reader's `flash_available` choice → always `v.lender_holding`, by `live_reader`;
  10. the engine's lender choice → always `BALANCER`, by `armed_engine`;
  11. `lender_code_ok`'s comparison → `true`, by `shadow_parts`.
- [ ] **Step 3: `PLAN.md`** gets an R27 entry after R26.3:
  - the cap evidence;
  - the Aave bug and its simulation;
  - the lender and its tests;
  - Task 2's three `PROOF` lines;
  - the deployed address and hash;
  - the mutants;
  - the deploy.
- [ ] **Step 4: Commit 1 (code)** by named paths:
  - every file this plan created or modified;
  - the spec and this plan;
  - not `out/` or `broadcast/`, except `broadcast/DeployMorphoFlashLender.s.sol/8453/run-latest.json` from the operator's broadcast, if it holds no secret.

  Check `git diff --stat -- <paths>` is empty. Verify in a clean worktree with its own `CARGO_TARGET_DIR`: `cargo test -j 3 -p apex-math -p apex-chain -p apex-runtime` and `forge test`.
- [ ] **Step 5: Merge and push** (with the operator's go-ahead for the redeploy). Then commit 2: `ops/shadow.base.yaml`'s `lenders.morpho` uncommented with the deployed address and code hash, after Task 7 Step 4. `the_shipped_config_parses` should then also assert `c.lenders.morpho.is_some()`; add that assertion in this commit.
- [ ] **Step 6: Redeploy the shadow** (a new 14-day clock) with `host-deploy` as for R26.3. Checks:
  - the boot log shows no "Morpho lender" error;
  - after 5 minutes, `ops/host/remote.sh` shows the `lenders` line with Morpho's holding;
  - arms labelled `morpho` with `size_wei` above Balancer's holding appear in `armed-*.jsonl`;
  - in the next spike, the evaluator's simulations of Morpho-funded fires agree with their quotes (`ops/host/remote.sh armed`: `sim_agreed`);
  - the reactive funnel shows no new decline class.
- [ ] **Step 7:** Tell the operator the new clock's start, and record it in memory (`shadow-run-operations`).
