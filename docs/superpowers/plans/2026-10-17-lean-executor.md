# R29 Lean Executor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A new, non-upgradeable executor that calls pools directly and keeps only the operator's four checks, so a two-hop trade burns ~40-50% less gas, and the canary trades through it.

**Architecture:** `contracts/lean/LeanExecutor.sol` takes a packed plan in `run(bytes)`, borrows WETH (Balancer V2 or Morpho Blue), swaps hop by hop paying each pool in its own callback (validated through transient storage), checks the floor, sends the profit to the recipient and repays. `apex-exec` gains a `lean` encoder whose output a shared fixture holds to the contract's decoder; the runtime's two plan builders, lender choice, gas model, settlement and boot check switch to it.

**Tech Stack:** Solidity 0.8.24 (Foundry, via-IR, EVM cancun), Rust (`apex-exec`, `apex-runtime`), anvil and `forge` forks of Base through `https://base.drpc.org`.

**Spec:** `docs/superpowers/specs/2026-10-17-lean-executor-design.md`

## Global Constraints

- Kept on chain: trader-only access, the plan deadline, the profit floor (loan + fee + `minProfit`, `minProfit` ≥ 1 wei in every plan built), the owner's circuit breaker. Hop minimums stay.
- Dropped on chain: the plan commitment hash, the adapter allowlist, the declared-residue balance invariant, the canonical profit token, the slippage config guard.
- Profit: one WETH transfer of the gross to `profitRecipient` per trade, and `event Profit(uint256 gross)`.
- Loans are WETH only (every live cycle starts and ends at WETH).
- Gas target: ≤ 340,000 gas used on ticket 664's route at Base block 52,671,989 (old: 566,972). A target, not a gate: report the breakdown if above.
- The view surface the runtime reads keeps today's signatures: `executors(address)`, `isCircuitOpen()`, `profitRecipient()`.
- Owner transactions are prepared and dry-run here; the owner broadcasts. `.env`'s `PRIVATE_KEY` is the owner's and is never loaded on a trading path. Never print secrets.
- Repository rules: never `cargo fmt`; cargo with `-j 3`; all five CI gates (`scratchpad/gates.sh`) before any commit is pushed; verify the commit in a clean worktree; stage named paths only; no new non-secret environment variables; ask before merge, push or deploy.
- Fork tests use `https://base.drpc.org` (it serves historical state; `base-rpc.publicnode.com` refuses archive reads without a token), never the canary's BlockPI key.

## Review Focus

1. **A flash loan someone else starts naming the executor as recipient** (Balancer lets anyone do this, with their own `userData`): must revert; only a loan `run` started may execute hops. Test in Task 3.
2. **A callback from anything but the pool being swapped right now** — a direct call, a second callback from the same pool, a token re-entering during a transfer: must revert and pay nothing. Tests in Task 3.
3. **A malformed plan** — length not `38 + 57n`, no hops, a last hop whose `tokenOut` is not WETH, an unknown hop kind or lender: must revert before any loan is taken. Tests in Task 3 (contract) and Task 6 (encoder refuses to build one).
4. **An intermediate token that delivers less than the pool reports** (fee-on-transfer, rebasing): the next hop's payment must fail rather than draw on the loan; the most it can take is the executor's dust. Test in Task 3.
5. **A deadline revert must stay cheap** (it was ~68k gas, and the canary classifies early reverts under `EARLY_REVERT_GAS` = 100,000): the deadline is checked before any loan. Measured in Task 5, asserted in Task 8.

---

## The plan's bytes (shared by Tasks 3, 6 and 7)

Big-endian, packed, no padding. `run(bytes plan)` is ABI-encoded as usual around it.

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | `lender`: 0 = Balancer V2, 1 = Morpho Blue |
| 1 | 16 | `amount`: WETH borrowed (uint128) |
| 17 | 5 | `deadline`: unix seconds (uint40); `block.timestamp > deadline` reverts |
| 22 | 16 | `minProfit` (uint128) |
| 38 + 57·i | 1 | hop `kind`: 0 = Uniswap v3 and both Slipstreams (`uniswapV3SwapCallback`), 1 = PancakeSwap v3 (`pancakeV3SwapCallback`), 2 = Aerodrome v2 |
| +1 | 20 | hop `pool` |
| +21 | 20 | hop `tokenOut`; its `tokenIn` is the previous hop's `tokenOut` (WETH for hop 0); `zeroForOne = tokenIn < tokenOut` |
| +41 | 16 | hop `minOut` (uint128): the exact quote; for kind 2 it is also the exact amount out requested |

Hop 0 spends `amount`; hop i+1 spends exactly what hop i returned. The last hop's `tokenOut` is WETH.

---

### Task 1: Solidity 0.8.24 and EVM cancun for the whole project

**Files:**
- Modify: `foundry.toml` (`solc_version = "0.8.24"`, add `evm_version = "cancun"`)

- [ ] **Step 1:** Change both settings. 0.8.24 is installed under `~/.local/share/svm`; no download.
- [ ] **Step 2:** Run `forge build` then `forge test`. Expected: the same suites pass as on `main` (143 tests, 23 suites). Run `scripts/ci/check_executor_size.sh`: ok.
- [ ] **Step 3:** Commit `foundry.toml`: `build(contracts): Solidity 0.8.24, EVM cancun, for transient storage (Task 8.5 R29)`.

### Task 2: `LeanExecutor`'s owner surface

**Files:**
- Create: `contracts/lean/LeanExecutor.sol`
- Test: `test/lean/LeanExecutorAdmin.t.sol`

**Interfaces:**
- Produces: `contract LeanExecutor` with
  - `constructor(address owner_, address trader_, address profitRecipient_)`, each non-zero (`ZeroAddress()`); sets `executors[trader_] = true`; approves WETH to Morpho Blue for `type(uint256).max` (its flash loan pulls repayment).
  - `address public owner`; `mapping(address => bool) public executors`; `address public profitRecipient` and `bool` circuit state packed in one slot; `function isCircuitOpen() external view returns (bool)`.
  - Owner-only (`NotOwner()`): `setExecutor(address,bool)`, `setProfitRecipient(address)`, `tripCircuit()`, `resetCircuit()`, `transferOwnership(address)`, `sweep(address token, address to, uint256 amount)`.
  - Constants: `WETH = 0x4200000000000000000000000000000000000006`, `BALANCER_VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8`, `MORPHO_BLUE = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb`.

- [ ] **Step 1: Write the failing tests:** `testConstructorSetsRoles` (owner, `executors(trader)`, `profitRecipient`, circuit closed), `testZeroAddressesAreRefused`, `testEveryAdminCallIsOwnerOnly` (each of the six from the trader reverts `NotOwner`), `testCircuitTripsAndResets`, `testSweepMovesTokens`, `testTheReaderSelectorsAreUnchanged`:
  ```solidity
  assertEq(LeanExecutor.executors.selector, bytes4(keccak256("executors(address)")));
  assertEq(LeanExecutor.isCircuitOpen.selector, bytes4(keccak256("isCircuitOpen()")));
  assertEq(LeanExecutor.profitRecipient.selector, bytes4(keccak256("profitRecipient()")));
  ```
- [ ] **Step 2:** `forge test --match-path test/lean/LeanExecutorAdmin.t.sol`: fails to compile.
- [ ] **Step 3:** Write the contract's owner surface.
- [ ] **Step 4:** The same command passes.
- [ ] **Step 5:** Commit both files: `feat(contracts): the lean executor's owner surface (Task 8.5 R29)`.

### Task 3: `run`, Balancer, the three hop kinds and the floor

**Files:**
- Create: `contracts/lean/LeanPlanCodec.sol` (library), `test/lean/LeanMocks.sol`, `test/lean/LeanExecutorRun.t.sol`
- Modify: `contracts/lean/LeanExecutor.sol`

**Interfaces:**
- Consumes: Task 2's contract.
- Produces:
  - `library LeanPlanCodec`: `function header(bytes calldata p) internal pure returns (uint8 lender, uint256 amount, uint256 deadline, uint256 minProfit, uint256 hopCount)` and `function hop(bytes calldata p, uint256 i) internal pure returns (uint8 kind, address pool, address tokenOut, uint256 minOut)`, reading the layout above; `header` reverts `BadPlan()` unless `p.length >= 95 && (p.length - 38) % 57 == 0`.
  - `function run(bytes calldata plan) external` on `LeanExecutor`: in order, `executors[msg.sender]` (`NotTrader()`), circuit (`CircuitOpen()`), deadline (`Expired(uint256 deadline, uint256 timestamp)`), plan shape (`BadPlan()`: lender ≤ 1, every kind ≤ 2, last `tokenOut == WETH`), then the loan.
  - `receiveFlashLoan(address[],uint256[],uint256[],bytes)`, `uniswapV3SwapCallback(int256,int256,bytes)`, `pancakeV3SwapCallback(int256,int256,bytes)`.
  - `event Profit(uint256 gross)`; errors `UnexpectedCallback()`, `HopShort(uint256 hop, uint256 out, uint256 minOut)`, `Floor(uint256 out, uint256 required)`.
- Transient slots (inline `tstore`/`tload`): the loan in flight (set by `run`, required by the lender callback, cleared after), the expected pool and the current `tokenIn` (set before each `swap`, checked and cleared by the swap callback; `run` reverts `UnexpectedCallback()` if a v3 hop returns with the expected pool still set).
- A v3 hop: `swap(address(this), zeroForOne, int256(amountIn), zeroForOne ? 4295128740 : 1461446703485210103287273052203988822378723970341, "")` (`MIN_SQRT_RATIO + 1` / `MAX_SQRT_RATIO - 1`); out is the negated delta of `tokenOut`. An Aerodrome v2 hop: transfer `amountIn` of `tokenIn` to the pool, then `swap(zeroForOne ? 0 : minOut, zeroForOne ? minOut : 0, address(this), "")`; out is `minOut`.
- Settlement inside the loan callback: `out >= amount + fee + minProfit` else `Floor`; transfer `out - amount - fee` WETH to `profitRecipient`; `emit Profit(out - amount - fee)`; transfer `amount + fee` to the vault.

- [ ] **Step 1: Write mocks** in `test/lean/LeanMocks.sol`: an ERC-20 (and one that delivers 1 unit less than asked on `transfer`), a v3 pool that pays out at a fixed rate and calls either callback by a constructor flag, an Aerodrome v2 pool (pays `amountOut` if it has received at least a fixed-rate input), a Balancer vault with `flashLoan` that calls `receiveFlashLoan` and checks repayment.
- [ ] **Step 2: Write the failing tests:**
  - `testTwoHopsPayTheProfitAndRepay`: WETH→X on a v3 mock, X→WETH on a pancake-flag mock; asserts the recipient's WETH rose by exactly `out - amount`, the vault holds its loan back, and `Profit(out - amount)` was emitted (`vm.expectEmit`).
  - `testAerodromeV2Hop`: one v2 hop in the cycle; same assertions.
  - `testEachHopSpendsWhatTheLastReturned`: hop 1's pool is asked for exactly hop 0's output (the mock records `amountSpecified`).
  - `testHopBelowItsMinimumReverts` (`HopShort(0, out, minOut)`), `testFloorReverts` (`Floor`), `testNotTraderReverts`, `testCircuitOpenReverts`.
  - `testExpiredRevertsBeforeAnyLoan`: `vm.warp(deadline + 1)`; reverts `Expired`, and the vault's `flashLoan` was never called.
  - Review Focus 1, `testAFlashLoanSomeoneElseStartsReverts`: the attacker calls the mock vault's `flashLoan` naming the executor with a valid-looking plan; reverts `UnexpectedCallback`.
  - Review Focus 2, `testACallbackFromOutsideATradeReverts`, `testACallbackFromTheWrongPoolReverts`, `testASecondCallbackReverts`: each reverts `UnexpectedCallback` and moves no tokens.
  - Review Focus 3, `testMalformedPlansRevertBeforeAnyLoan`: 94 bytes, 96 bytes, zero hops, last `tokenOut` not WETH, kind 3, lender 2; each reverts `BadPlan` and the vault was never called.
  - Review Focus 4, `testAShortDeliveringTokenCannotDrawOnTheLoan`: the middle token delivers 1 less; with no dust the trade reverts; with 5 units of dust held by the executor it can complete and the executor ends with 4.
- [ ] **Step 3:** `forge test --match-path test/lean/LeanExecutorRun.t.sol`: fails.
- [ ] **Step 4:** Implement `LeanPlanCodec` and `run`.
- [ ] **Step 5:** The same command passes; then `forge test` whole: passes.
- [ ] **Step 6:** Commit the four files: `feat(contracts): the lean executor's run path (Task 8.5 R29)`.

### Task 4: Morpho Blue as the second lender

**Files:**
- Modify: `contracts/lean/LeanExecutor.sol`, `test/lean/LeanMocks.sol`, `test/lean/LeanExecutorRun.t.sol`

**Interfaces:**
- Produces: lender byte 1 calls `IMorphoFlashLoan(MORPHO_BLUE).flashLoan(WETH, amount, plan)`; `onMorphoFlashLoan(uint256 assets, bytes calldata data)` requires `msg.sender == MORPHO_BLUE` and the loan in flight, runs the hops, settles as Task 3 with fee 0, and leaves repayment to Morpho's `transferFrom` (the constructor's approval).

- [ ] **Step 1: Failing tests** (a mock Morpho at `MORPHO_BLUE` via `vm.etch`, pulling repayment with `transferFrom` after the callback): `testMorphoLoanPaysTheProfitAndIsRepaid`, `testMorphoCallbackFromOutsideATradeReverts`.
- [ ] **Step 2:** Run them: fail. **Step 3:** Implement. **Step 4:** Pass, whole `forge test` passes.
- [ ] **Step 5:** Commit: `feat(contracts): Morpho Blue loans in the lean executor (Task 8.5 R29)`.

### Task 5: Measured on a fork of Base

**Files:**
- Create: `test/fork/LeanExecutorFork.t.sol`
- Modify: `foundry.toml`: the default profile skips `test/fork/*`; `[profile.fork]` runs only it. Verify both: `forge test` runs none of it, `FOUNDRY_PROFILE=fork forge test` runs all of it.

- [ ] **Step 1: Write the fork tests** (`vm.createSelectFork("https://base.drpc.org", 52_671_989)`; deploy `LeanExecutor` with the test contract as owner and trader):
  - `testTicket664RouteIsExactAndLean`: lender 0, `amount = 1_937_500_000_000_000_000`; hop 0: kind 0, pool `0x7C7420DD105E2779316423Ba3E973f434315EFA9`, tokenOut cbBTC `0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf`, minOut `5_782_309`; hop 1: kind 0, pool `0x8c7080564B5A792A33Ef2FD473fbA6364d5495e5`, tokenOut WETH, minOut `1_937_500_000_000_000_001`; `minProfit = 1`. Asserts the recipient receives exactly `180_256_364_565_428` wei (the old executor's gross at this block). Measures execution gas (`gasleft` around `run`) cold and with dust (`deal` 1 unit of cbBTC to the executor and 1 wei of WETH to the recipient), and the old executor's `startV2` with ticket 664's own calldata from the trader `0xCB436Ba3acb945b3fc8EE6345857262584356595` at the same block (`vm.prank`) for the same measure; logs all three; asserts lean < old.
  - `testBalancerAgainstMorpho`: the same route with lender 1; logs both gases and the gross (equal).
  - `testEachVenueSwapsAsItsRouterDoes`: for each kind on a real pool (Uniswap 0.05% WETH/USDC `0xd0b53D9277642d899DF5C87A3966A349A798F224`; PancakeSwap 1 bp `0x72AB388E2E2F6FaceF59E3C3FA2C4E29011c2D38`; Slipstream ts100 WETH/USDC `0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59`; Slipstream V3 ts1 WETH/cbBTC `0x7C7420DD105E2779316423Ba3E973f434315EFA9`; Aerodrome v2 WETH/USDC `0xcDAC0d6c6C59727a65F871236188350531885C43`), the output of 0.1 WETH through the venue's router (Uniswap SwapRouter02 `0x2626664c2603336E57B271c5C0b26F421741e481`, the others from `crates/apex-runtime/src/live/frontier.rs`), under `vm.snapshotState`/`vm.revertToState`, equals the output of the same input through a test-only subclass of `LeanExecutor` that exposes one hop.
  - `testAnExpiredRunIsCheap`: `vm.warp` past the deadline; logs the gas of the revert.
- [ ] **Step 2:** `FOUNDRY_PROFILE=fork forge test -vv`: all pass. Record the logged numbers in the commit message; they set Task 7's constants.
- [ ] **Step 3:** Commit both files with the numbers: `test(contracts): the lean executor on a fork of Base (Task 8.5 R29)`.

### Task 6: The plan's encoder in `apex-exec`, held to the contract by a fixture

**Files:**
- Create: `crates/apex-exec/src/lean.rs`, `crates/apex-exec/tests/lean_plan.rs`, `crates/apex-exec/tests/fixtures/lean_plans.json`, `test/lean/LeanPlanFixture.t.sol`
- Modify: `crates/apex-exec/src/lib.rs`, `crates/apex-exec/src/call.rs`

**Interfaces:**
- Produces (`apex_exec::lean`):
  ```rust
  pub enum LeanLender { Balancer = 0, Morpho = 1 }
  pub enum HopKind { UniswapV3Style = 0, PancakeV3 = 1, AerodromeV2 = 2 }
  pub struct LeanHop { pub kind: HopKind, pub pool: Address, pub token_out: Address, pub min_out: u128 }
  pub struct LeanPlan { pub lender: LeanLender, pub amount: u128, pub deadline: u64, pub min_profit: u128, pub hops: Vec<LeanHop> }
  pub const LEAN_PLAN_VERSION: u32 = 3;
  pub const RUN_SELECTOR: [u8; 4]; // keccak256("run(bytes)")[..4]
  pub fn encode_plan(plan: &LeanPlan) -> Result<Vec<u8>, LeanError>;
  pub fn run_calldata(plan: &LeanPlan) -> Result<Vec<u8>, LeanError>;
  pub fn lean_commitment(plan: &LeanPlan, chain_id: u64, executor: Address) -> Result<B256, LeanError>;
  pub struct SignedLeanPlan; // ::check(plan, declared, chain_id, executor) -> Result<Self, LeanError>
  pub enum LeanError { NoHops, DeadlineTooLarge(u64), LastHopNotWeth, CommitmentMismatch { declared: B256, recomputed: B256 } }
  ```
  `lean_commitment` is off-chain only (INV-06's signer half): `keccak256(abi.encode(chain_id, executor, uint8(3), keccak256(encode_plan(plan))))`.
- `ExecutorCall`: a second constructor `pub fn from_lean(checked: SignedLeanPlan) -> Self`; its plan becomes `pub enum CallPlan { V2(PlanV2), Lean(LeanPlan) }` and `plan()` returns `&CallPlan`. `to()`, `chain_id()`, `data()` and `commitment()` keep their signatures.
- The fixture: hand-written JSON (three plans: Balancer two v3 hops; Morpho with a pancake and an Aerodrome hop; a three-hop plan), each with its fields and the expected `encode_plan` hex. Rust and Solidity both read it; neither writes it.

- [ ] **Step 1: Failing Rust tests** in `lean_plan.rs`: `every_fixture_plan_encodes_to_its_bytes`, `run_calldata_wraps_the_plan_as_abi_bytes` (selector, offset 0x20, length, padded bytes), `the_encoder_refuses_what_the_contract_would` (no hops, deadline above 2⁴⁰−1, last hop not WETH), `the_commitment_moves_with_every_field`, `a_signed_plan_with_another_commitment_is_refused`, `an_executor_call_from_a_lean_plan_carries_its_calldata`.
- [ ] **Step 2: Failing forge test** `testEveryFixturePlanDecodesToItsFields` in `LeanPlanFixture.t.sol`: reads the JSON (`vm.readFile`, `vm.parseJson*`), decodes each plan's bytes through a harness over `LeanPlanCodec`, asserts every field.
- [ ] **Step 3:** Run `cargo test -j 3 -p apex-exec --test lean_plan` and `forge test --match-path test/lean/LeanPlanFixture.t.sol`: both fail.
- [ ] **Step 4:** Implement `lean.rs` and `from_lean`; adjust `.plan()`'s callers in `crates/apex-exec/tests/executor_call.rs`, `crates/apex-runtime/tests/armed_predicate.rs`, `crates/apex-runtime/tests/live_calls.rs` to match `CallPlan::V2`.
- [ ] **Step 5:** Both commands pass; `cargo test -j 3 -p apex-exec --all-targets` passes.
- [ ] **Step 6:** Commit: `feat(exec): the lean plan's encoder, held to the contract by a fixture (Task 8.5 R29)`.

### Task 7: The runtime builds lean plans

**Files:**
- Modify: `crates/apex-runtime/src/live/calls.rs`, `crates/apex-runtime/src/armed/plan.rs`, `crates/apex-runtime/src/live/lenders.rs`, `crates/apex-runtime/src/live/admission.rs` (`GAS_PER_HOP`), `crates/apex-runtime/src/shadow/mod.rs` (boot: venues, lenders), `crates/apex-runtime/src/shadow/config.rs` (`LendersConfig`)
- Test: `crates/apex-runtime/tests/live_calls.rs`, `crates/apex-runtime/tests/armed_predicate.rs`, `crates/apex-runtime/tests/live_lenders.rs` (or wherever lender choice is tested today)

**Interfaces:**
- Consumes: Task 6's `LeanPlan`, `LeanHop`, `HopKind`, `LeanLender`, `lean_commitment`, `SignedLeanPlan`, `ExecutorCall::from_lean`.
- Produces:
  - `pub fn lean_hop(leg: &Leg, min_out: U256) -> Result<LeanHop, String>` in `live/calls.rs`, replacing `encode_step`: kind 0 for `UniswapV3 | Slipstream | SlipstreamV3`, 1 for `PancakeV3`, 2 for `AerodromeV2`; pool `leg.pool`; `token_out` `leg.token_out`.
  - `LiveCallBuilder::build` and `armed::plan::armed_call` return `ExecutorCall::from_lean(..)`; hop minimums as today (hop 0: the quoted `mid`; hop 1: `max(floor, slippage bound)` in the reactive plan, loan + 1 in the armed one).
  - `LenderChoice { id: FlashProviderId, lender: LeanLender, gas: u64 }`; Balancer (`BALANCER_FLASH`) and Morpho Blue direct (`MORPHO_FLASH`, no wrapper); the choice is the cheapest lender whose holding covers the size; both gas figures and `GAS_PER_HOP` are Task 5's measurements, each cited in its doc comment.
  - `AdapterBinding`, `binding` and `reachable_venues` are deleted; the boot's universe is every `Venue` (the contract supports all five by its code). `lenders.morpho` leaves `LendersConfig`; Morpho Blue's WETH holding is read at boot as today.

- [ ] **Step 1: Failing tests:** `a_cycle_builds_one_run_call_with_its_exact_hops` (decode `ExecutorCall::data()`: selector `RUN_SELECTOR`, then the plan bytes equal `encode_plan` of the expected `LeanPlan` from the book's pools), `each_venue_maps_to_its_hop_kind`, `the_armed_call_is_a_lean_plan_with_a_floor_of_one`, `the_cheapest_lender_that_covers_the_size_is_chosen` (with Balancer the cheaper: Balancer under its holding, Morpho above it; with Morpho the cheaper: Morpho at every size; neither when neither covers).
- [ ] **Step 2:** `cargo test -j 3 -p apex-runtime --test live_calls --test armed_predicate`: fails.
- [ ] **Step 3:** Implement; delete what the Interfaces say is deleted; fix every caller the compiler names.
- [ ] **Step 4:** `cargo test -j 3 -p apex-runtime --all-targets --no-fail-fast`: passes.
- [ ] **Step 5:** Commit: `feat(runtime): plans for the lean executor (Task 8.5 R29)`.

### Task 8: Settlement, the boot check and the armed evaluator read the new event

**Files:**
- Modify: `crates/apex-runtime/src/canary/chain.rs` (`PROFIT_REALISED` → the `Profit(uint256)` topic; reconcile the one WETH transfer executor → recipient against it), `crates/apex-runtime/src/shadow/mod.rs` (the boot check reads `profitRecipient()` only), `crates/apex-runtime/src/armed/run.rs` (the simulated gross from `Profit(uint256)`), `crates/apex-runtime/src/live/abi.rs` (drop `FEE_RECIPIENT` if nothing else reads it)
- Test: `crates/apex-runtime/tests/canary_chain.rs`, `crates/apex-runtime/tests/shadow.rs`

- [ ] **Step 1: Failing tests:** `a_landed_trade_reconciles_its_transfer_against_profit` (receipt fixture with the new event), `a_profit_event_that_disagrees_with_the_transfer_is_unexplained`, `the_canary_refuses_to_start_unless_profit_goes_to_its_recipient` (one view read), `the_expired_run_reverts_under_the_early_bound` (Task 5's measured gas < `EARLY_REVERT_GAS`).
- [ ] **Step 2:** Run them: fail. **Step 3:** Implement. **Step 4:** `cargo test -j 3 -p apex-runtime --all-targets --no-fail-fast`: passes.
- [ ] **Step 5:** Commit: `feat(canary): settle against the lean executor's profit event (Task 8.5 R29)`.

### Task 9: The fork proof through the canary's own path

**Files:**
- Modify: `crates/apex-runtime/tests/canary_fork.rs`

- [ ] **Step 1:** Change the ignored fork proof to deploy `LeanExecutor` on the anvil fork from `out/LeanExecutor.sol/LeanExecutor.json` (owner impersonated, a dev trader authorized, recipient set), build the trade with `LiveCallBuilder`, send it with the canary's sender, settle and reconcile with `CanaryChain`. Assert: realised gross equals the prediction to the wei; a stale plan reverts under `EARLY_REVERT_GAS`; the stop file stops a send.
- [ ] **Step 2:** `forge build` then `cargo test -j 3 -p apex-runtime --test canary_fork -- --ignored --nocapture`: passes; note the gross and gas in the commit message.
- [ ] **Step 3:** Commit: `test(canary): the fork proof through the lean executor (Task 8.5 R29)`.

### Task 10: The owner's transactions and the runbook

**Files:**
- Create: `script/DeployLeanExecutor.s.sol`, `script/SeedLeanDust.s.sol`
- Test: `test/lean/DeployLeanExecutor.t.sol`
- Modify: `ops/host/README.md` (new §11)

**Interfaces:**
- `DeployLeanExecutor.run(address owner, address trader)`: deploys with `profitRecipient = owner`, logs the address and `keccak256` of its runtime code (the configuration's `code_hash`).
- `SeedLeanDust.run(address executor, address[] tokens, address[] routers, bytes[] swaps)`: as the owner, wraps a few wei of ETH, keeps 1 wei of WETH at the owner, and for each token runs its prepared router swap (a few units bought with WETH) and sends what it bought to the executor. The tokens are every non-WETH token in the canary's universe at deploy time, each bought through its deepest WETH pool's venue router (`frontier.rs` holds the routers' addresses).

- [ ] **Step 1: Failing test** `testDeployScriptConfiguresTheExecutor` (runs the script in-process; asserts owner, trader authorized, recipient, logged code hash equals the deployed code's).
- [ ] **Step 2:** Run it: fails. **Step 3:** Write both scripts. **Step 4:** Passes.
- [ ] **Step 5:** Dry-run both against Base through `https://mainnet.base.org` (`forge script … --rpc-url … --sender <owner>`, no `--broadcast`); record the gas. Write README §11: deploy (owner, `--interactive`), read back the code hash, seed dust, update `ops/canary.base.yaml` and `ops/shadow.base.yaml` (`executor.address`, `executor.code_hash`, `executor.plan_version: 3`, the `lenders` section removed), deploy to the host, then revoke the trader on the old executor through its router (`multicall` of `setExecutor(trader,false)`, dry-run with `cast call --from <owner>` first).
- [ ] **Step 6:** Commit: `feat(ops): deploy and seed the lean executor (Task 8.5 R29)`.

### Task 11: Finish

- [ ] **Step 1:** PLAN.md: an R29 entry: why, the measured breakdown before and after, the dropped and kept checks as the operator decided, the lender measured, and what the owner must do.
- [ ] **Step 2:** All five gates; a mutation sweep over every guard in `LeanExecutor.run`, the callbacks, `LeanPlanCodec.header` and the Rust encoder's refusals (a surviving mutant is a missing test, or a guard to delete); verify the branch's last commit in a clean worktree.
- [ ] **Step 3:** Commit PLAN.md. Then stop: the owner deploys (README §11), and only after the operator approves are the configuration change, merge, push and host deploy made, as one more commit.
