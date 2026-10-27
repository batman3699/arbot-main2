# Armed lane v3 (v4 pools and USDC routes) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Arm the census's two-hop Uniswap v4 and USDC routes as validity transactions, through one new executor, in shadow first.

**Architecture:**
- A second lean executor (`LeanExecutor2`) and its plan codec add a start token (WETH or USDC), a Uniswap v4 hop, and PoolManager flash accounting as a lender.
- The runtime loads v4 pools from PoolManager storage, under a 20-byte key cut from the pool ID, so every address-keyed map is unchanged.
- It prices them with the existing concentrated-liquidity ladder and follows them through a second, pool-ID-filtered log subscription.
- Cycles are built from USDC as well as WETH, with costs converted into the start token.
- The census, the arming engine and the evaluator learn v4 slots and USDC routes.
- Nothing goes live until gate A″ passes and the operator approves the deploy.

**Tech Stack:** Solidity 0.8.24 and foundry; Rust (`apex-exec`, `apex-runtime`, `apex-venues`, `apex-chain`); Python 3.12 (`scripts/data/arb_census`).

**Spec:** `docs/superpowers/specs/2026-10-28-v4-usdc-armed-lane-design.md` (approved 2026-10-28).

## Global Constraints

- **Secrets:** never printed, echoed or logged (`BLOCKPI_KEY`, `PRIVATE_KEY`, `TRADER_PRIVATE_KEY`, `APEX_SECRET_*`). Keys go by file or environment, never argv.
- **The repo's habits:**
  - never run `cargo fmt`;
  - cargo runs with `-j 3`;
  - stage named paths, never `git add -A`;
  - no new non-secret environment variables (config-file fields are fine).
- **Before any push:** all five gates.
  - Every `scripts/ci/*.sh`; `check_placeholder_endpoints.sh` and `cl_parity_sweep.sh` are known red and unwired.
  - Clippy over CI's crate list with `--all-targets -- -D warnings`.
  - `cargo test --workspace --all-targets`.
  - `cargo test --workspace --doc`.
  - `forge test`.
- **The operator approves** each merge, push, deploy and owner broadcast. Owner transactions are dry-run by the agent and broadcast by the operator.
- **The deployed v1 stays untouched:** `LeanExecutor` (0xd25B…f2c5), `LeanPlanCodec`, `apex_exec::lean` and `LEAN_PLAN_VERSION = 3`. The reactive lane and the deployed code hash depend on them.
- **The new executor's tokens:**
  - start tokens are only WETH `0x4200000000000000000000000000000000000006` and USDC `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`;
  - PoolManager is `0x498581fF718922c3f8e6A244956aF099B2652b2b`;
  - Morpho Blue is `0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb`.
- **v4 storage (verified on chain 2026-10-28):**
  - a pool's state slot is `keccak256(abi.encode(poolId, uint256(6)))`;
  - slot0 there packs sqrtPriceX96 (bits 0–159), tick (160–183, signed), protocolFee (184–207) and lpFee (208–231);
  - liquidity is at state + 3, ticks at + 4 and the tick bitmap at + 5.
- **v4 event topics:**
  - `Swap` `0x40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f`;
  - `ModifyLiquidity` `0xf208f4912782fd25c7f114ca3723a2d5dd6f3bcc3ac8db5af63baa85f711d5ec`;
  - `Initialize` `0xdd466e674ea557f56295e2d0218a125ea4b4f0f6f3307b95f85e6110838d6438`.
- **The test oracle only:** StateView `0xA3c0c9b65baD0b08107Aa264b0f3dB444b867A71`. It agreed with direct storage reads to the wei on 2026-10-28.
- **Pool admission refuses:**
  - a native-ETH currency (the zero address);
  - a dynamic fee (`fee & 0x800000 != 0`);
  - any hook bit among `BEFORE_SWAP_RETURNS_DELTA` (1<<3), `AFTER_SWAP_RETURNS_DELTA` (1<<2), `AFTER_ADD_LIQUIDITY_RETURNS_DELTA` (1<<1) and `AFTER_REMOVE_LIQUIDITY_RETURNS_DELTA` (1<<0);
  - a nonzero hook not in `ops/v4-hooks.base.json`;
  - a pool whose 20-byte key equals another inventoried pool's address.
- **No v4 log subscription without a topic-1 filter on pool IDs.**
- **No archive log sweep on BlockPI.** The daily v4 inventory scan uses public endpoints for the day's blocks only.
- **Size caps:** `armed.max_size_wei` (WETH, unchanged) and a new `armed.max_size_usdc` = 12,000 USDC (`12_000_000_000` in 6-decimal units).
- **Denim:** nothing new may depend on flashblocks or `pendingLogs`.

## Rulings this plan makes (beyond the spec)

- **A v4 hop runs only under lender 2 (PoolManager).** A plan with a kind-3 hop and any other lender is refused. The spec chose PoolManager lending for v4 routes, and refusing the mix removes a second unlock path. *Cost if wrong:* plans can't fall back to Morpho without a codec change.
- **Each v4 hop settles at once**, inside the trade's unlock: swap, sync, transfer, settle, take. Netting deltas across hops is left out. *Cost if wrong:* some gas a hop, which the fork test measures.
- **Pools with a nonzero protocol fee are refused, at admission and at every read.** The spec asks for the protocol fee composed by direction. Every top pool reads protocolFee 0 today (2026-10-28), and refusing is exact where the composition would put a direction-specific fee into `ClPoolState`'s single `fee_ppm`. *Cost if wrong:* those pools are skipped until a follow-up adds the composition.
- **The `Profit(uint256)` event keeps its signature.** Topic `0x357d905f…bc8` is the one existing readers watch, and the token comes from the plan's start. *Cost if wrong:* a reader must know the start token, and the arm always does.
- **A kind-3 hop reuses the hop layout.** Its `pool` field holds the hook address, followed by 6 more bytes: fee (uint24), then tick spacing (int24). The currencies follow from tokenIn and tokenOut, lower address first.

## Review Focus

1. **A negative tick, or a tick on a bitmap word's edge.** `abi.encode(int16)` and `abi.encode(int24)` sign-extend to 32 bytes. A slot computed from the unsigned value reads an empty word and silently drops liquidity. Pinned in Task 5 (`v4_slots_sign_extend_negative_keys`).
2. **The sign of a v4 Swap event's amounts.** They are the swapper's deltas, the negative of v3's. A decoder that copies v3 gets every notional and direction backwards. Pinned in Task 9 (`a_v4_swap_s_amounts_are_the_pool_s_side`).
3. **USDC's 6 decimals in the economics.** A cost in wei converted to USDC units at the WETH/USDC mid price must keep the arm's surplus exact to one unit. A 10^12 slip makes every USDC arm look profitable or every one look dead. Pinned in Task 10 (`a_usdc_arm_s_costs_are_converted_at_the_mid_price`).
4. **A protocol fee switched on after admission.** The pool must leave the book at its next read, not be priced at the LP fee alone. Pinned in Task 7 (`a_v4_pool_whose_protocol_fee_turns_on_is_refused_at_read`).
5. **A ModifyLiquidity delta outside `i128`.** It must refuse the in-place apply and reload the pool, not wrap. Pinned in Task 9 (`a_modify_liquidity_beyond_i128_reloads_the_pool`).

---

## Phase 1: the executor

### Task 1: Plan-2 codec, Rust and Solidity, held together by a fixture

**Files:**
- Create: `crates/apex-exec/src/lean2.rs`, `contracts/lean/LeanPlan2Codec.sol`, `crates/apex-exec/tests/lean2_plan.rs`, `crates/apex-exec/tests/fixtures/lean2_plans.json`, `test/lean/Lean2PlanFixture.t.sol`
- Modify: `crates/apex-exec/src/lib.rs` (`pub mod lean2;`), `crates/apex-exec/src/call.rs` (`CallPlan::Lean2`, `ExecutorCall::from_lean2`)

**Interfaces:**
- Produces, Rust:
  - `pub const LEAN2_PLAN_VERSION: u32 = 4;`
  - `pub const USDC: Address` (`0x833589…2913`)
  - `pub enum StartToken { Weth = 0, Usdc = 1 }`, with `pub const fn address(self) -> Address`
  - `pub enum Lean2Lender { Balancer = 0, Morpho = 1, PoolManager = 2 }`
  - `pub enum Hop2Kind { UniswapV3Style = 0, PancakeV3 = 1, AerodromeV2 = 2, UniswapV4 = 3 }`
  - `pub struct V4HopKey { pub fee: u32, pub tick_spacing: i32, pub hooks: Address }`
  - `pub struct Lean2Hop { pub kind: Hop2Kind, pub pool: Address, pub token_out: Address, pub min_out: u128, pub v4: Option<V4HopKey> }`. For kind 3, `pool` is ignored on encode: the hook address is written there.
  - `pub struct Lean2Plan { pub lender: Lean2Lender, pub start: StartToken, pub amount: u128, pub deadline: u64, pub min_profit: u128, pub hops: Vec<Lean2Hop> }`
  - `pub fn encode_plan2(&Lean2Plan) -> Result<Vec<u8>, Lean2Error>`, `pub fn run2_calldata(&Lean2Plan) -> Result<Vec<u8>, Lean2Error>` (selector `run(bytes)`, as v1)
  - `pub fn lean2_commitment(&Lean2Plan, chain_id: u64, executor: Address) -> Result<B256, Lean2Error>`
  - `pub struct SignedLean2Plan`, with `check` and its accessors as v1's
  - `ExecutorCall::from_lean2(SignedLean2Plan) -> ExecutorCall` and `CallPlan::Lean2(Lean2Plan)`
- Produces, Solidity: `library LeanPlan2Codec`, with
  - `header(bytes calldata) returns (uint8 lender, uint8 start, uint256 amount, uint256 deadline, uint256 minProfit)`;
  - `hopAt(bytes calldata, uint256 offset) returns (uint8 kind, address pool, address tokenOut, uint256 minOut, uint24 fee, int24 tickSpacing, uint256 next)`;
  - `HEADER = 39`, `HOP = 57`, `V4_EXTRA = 6`.
- The bytes, big-endian and packed:
  - header: lender (1), start (1), amount (16), deadline (5), minProfit (16);
  - each hop: kind (1), pool or hook (20), tokenOut (20), minOut (16), then for kind 3 only fee (3) and tickSpacing (3, two's complement).

- [ ] **Step 1: Write the failing tests.** In `crates/apex-exec/tests/lean2_plan.rs`:
  - `every_fixture_plan_encodes_to_its_bytes`: each plan in `lean2_plans.json` encodes to its `hex`.
  - `a_v4_hop_carries_its_hook_fee_and_spacing`: a USDC-start plan with hop 0 of kind 3 (`fee 100, tick_spacing 1, hooks 0xb0b24b89db0dafbe43c5b40226b63a179f592880`, token out AXIS `0x5be75e3b0aff5d1a042a5363ae36acb1d6d755ea`) and hop 1 of kind 0.
    - `bytes[0..2] == [2, 1]`;
    - bytes 40..60 are the hook;
    - bytes 96..99 are `0x000064`;
    - bytes 99..102 are `0x000001`;
    - total length 39 + 63 + 57.
  - `a_plan_must_end_in_its_start_token`: a WETH-start plan whose last hop ends in USDC returns `Err(Lean2Error::LastHopNotStart)`.
  - `a_v4_hop_needs_the_pool_manager_lender`: kind 3 under `Lean2Lender::Morpho` returns `Err(Lean2Error::V4NeedsPoolManager)`.
  - `a_negative_tick_spacing_round_trips` (−1 encodes as `0xffffff`).
  - `the_commitment_names_version_four`: the commitment differs from v1's for the same fields.
  - In `test/lean/Lean2PlanFixture.t.sol`, `testEveryFixturePlanDecodesToItsFields` walks each fixture with `hopAt` and asserts every field.
- [ ] **Step 2: Run them to see them fail.**
  - Run: `cargo test -j 3 -p apex-exec --test lean2_plan` and `forge test --match-path test/lean/Lean2PlanFixture.t.sol`.
  - Expected: compile errors: `lean2` and `LeanPlan2Codec` don't exist.
- [ ] **Step 3: Implement `lean2.rs`, `LeanPlan2Codec.sol` and the fixture.**
  - The fixture holds 5 plans:
    - WETH start under Morpho, kinds 0 and 1;
    - USDC start under Morpho, kinds 1 and 0;
    - USDC start under PoolManager, kinds 3 and 0;
    - USDC start under PoolManager, kinds 0 and 3;
    - WETH start under Balancer, kind 2 in either hop.
  - The Rust test writes the hex; the Solidity test reads it.
- [ ] **Step 4: Run both to see them pass** (same commands; expected PASS).
- [ ] **Step 5: Commit.**
  - Stage `crates/apex-exec/src/lean2.rs`, `crates/apex-exec/src/lib.rs`, `crates/apex-exec/src/call.rs`, `crates/apex-exec/tests/lean2_plan.rs`, `crates/apex-exec/tests/fixtures/lean2_plans.json`, `contracts/lean/LeanPlan2Codec.sol` and `test/lean/Lean2PlanFixture.t.sol`.
  - Message: `feat(exec): plan 2, start token, v4 hop and PoolManager lender (Task 8.5 R37)`.

### Task 2: `LeanExecutor2`

**Files:**
- Create: `contracts/lean/LeanExecutor2.sol`, `test/lean/LeanExecutor2Run.t.sol`, `test/lean/LeanExecutor2Admin.t.sol`
- Modify: `test/lean/LeanMocks.sol` (add `MockPoolManager`, and a USDC-like 6-decimal `MockToken`)

**Interfaces:**
- Consumes: `LeanPlan2Codec` (Task 1).
- Produces:
  - `contract LeanExecutor2`, with v1's admin surface unchanged: `owner`, `executors`, `profitRecipient`, `isCircuitOpen`, `setExecutor`, `setProfitRecipient`, `tripCircuit`, `resetCircuit`, `transferOwnership`, `sweep`;
  - `run(bytes plan)`, `unlockCallback(bytes) returns (bytes)`, v1's swap and flash callbacks, and `event Profit(uint256 gross)`;
  - `constant USDC` and `constant POOL_MANAGER`. The constructor approves WETH and USDC to Morpho Blue.
- Behaviour:
  - `run` checks, before any loan:
    - the trader;
    - the circuit;
    - the deadline;
    - lender ≤ 2 and start ≤ 1;
    - every kind ≤ 3;
    - kind 3 only under lender 2;
    - the last tokenOut is the start token.
  - Lender 2 calls `POOL_MANAGER.unlock(plan)`.
  - `unlockCallback` serves only PoolManager while `T_LOAN` is set. It:
    - takes `amount` of the start token;
    - runs the hops;
    - checks the floor;
    - pays `gross` to `profitRecipient`;
    - repays with `sync(start)`, a transfer of `amount`, then `settle()`.
  - A kind-3 hop calls `POOL_MANAGER.swap(key, (zeroForOne, -int256(amountIn), limit), "")`:
    - the limit is v1's `MIN_LIMIT`/`MAX_LIMIT`;
    - the key's currencies are tokenIn and tokenOut, lower address first;
    - then it runs `sync(tokenIn)`, transfers `amountIn`, `settle()`, and `take(tokenOut, this, out)`;
    - `out` is the positive side of the returned `BalanceDelta` (high 128 bits amount0, low 128 bits amount1).

- [ ] **Step 1: Write the failing tests.** In `test/lean/LeanExecutor2Run.t.sol`, mirroring `LeanExecutorRun.t.sol`'s mocks:
  - `testUsdcStartUnderMorphoPaysTheProfitInUsdc`: recipient's USDC balance += gross, `Profit(gross)` emitted, Morpho repaid.
  - `testWethStartIsV1sTrade`: the same route and amounts as v1's `testTwoHopsPayTheProfitAndRepay` give the same gross.
  - `testV4HopUnderThePoolManager`: USDC → AXIS-like (kind 3, mock pool manager at a fixed price) → USDC (kind 0). The mock manager ends with zero net delta, and the recipient gets the gross.
  - `testV4FirstOrSecond`: either position works.
  - `testV4HopUnderMorphoIsRefusedBeforeAnyLoan`: reverts `BadPlan()`; no loan event in the mock.
  - `testUnlockCallbackFromAnyoneElseReverts` and `testUnlockCallbackOutsideATradeReverts`: `UnexpectedCallback()`.
  - `testLastHopNotTheStartReverts`, `testFloorRevertsInUsdc`, `testHopShortOnAV4Hop`.
  - `testTheUnlockReturnsWithNothingOwed`: the mock asserts every delta is settled.
  - In `test/lean/LeanExecutor2Admin.t.sol`, v1's admin tests, retargeted. `testTheReaderSelectorsAreUnchanged` covers `isCircuitOpen`, `executors` and `profitRecipient`.
- [ ] **Step 2: Run them to see them fail.**
  - Run: `forge test --match-path 'test/lean/LeanExecutor2*'`.
  - Expected: compile error, `LeanExecutor2` doesn't exist.
- [ ] **Step 3: Implement `LeanExecutor2.sol`, and `MockPoolManager` in `LeanMocks.sol`.**
  - The mock enforces the unlock: only inside `unlock`, and every delta zero on return.
  - `swap` prices at a settable fixed rate.
  - `take`, `sync` and `settle` track balances as v4-core does.
- [ ] **Step 4: Run them to see them pass.** Run `forge test --match-path 'test/lean/*'`. Expected: all pass, v1's included.
- [ ] **Step 5: Commit** `contracts/lean/LeanExecutor2.sol` and the three test files: `feat(contracts): LeanExecutor2, USDC start, v4 hop under PoolManager flash accounting (Task 8.5 R37)`.

### Task 3: Fork tests on AXIS, and the v4 hop's gas

**Files:**
- Create: `test/fork/LeanExecutor2Fork.t.sol`

**Interfaces:**
- Consumes: `LeanExecutor2` (Task 2) and `LeanPlan2Codec` (Task 1).
- Produces: the measured gas of a kind-3 hop on the hooked pool and on a hookless one, for Task 8's `gas::LEAN`. Record both in the commit message and in `PLAN.md`'s R37 entry.

- [ ] **Step 1: Write the fork tests.**
  - The fork is `vm.createSelectFork("https://base.drpc.org", BLOCK)`, as `LeanExecutorFork.t.sol` does. `BLOCK` is pinned at or after 53,155,787, where AXIS state was read.
  - The pools:
    - AXIS `0x5be75e3b0aff5d1a042a5363ae36acb1d6d755ea`;
    - the hooked v4 pool `0xc19d91d99afc0332e5013a569c9721cd9ecb51a14c851f7a189a140950a627fa`, key `(AXIS, USDC, 100, 1, 0xb0b24b89db0dafbe43c5b40226b63a179f592880)`;
    - of the two Uniswap v3 AXIS/USDC pools, `0xe1bfc5ada1125718b53b2affc638ba9981f6070f` and `0xe381d2ec7996d9fa1a72e461fb4470db9230f37a`, whichever has more liquidity at `BLOCK`.
  - Each test first opens a gap. It `deal`s USDC to a funded account and swaps on one pool, playing the trigger.
  - The tests:
    - `testAxisV4ThenV3`: USDC → AXIS on v4 (kind 3), then AXIS → USDC on v3 (kind 0), under lender 2. Assert the recipient's USDC increase equals the `Profit` event, and nothing is left in the executor.
    - `testAxisV3ThenV4`: the other direction.
    - `testV4HopGas`: `gasleft()` around one kind-3 hop, on the hooked pool and on the hookless pool `0x69241b4b4cb3ad6f3c0f4a4571051b04f0580b2175597053a1f206fb18381686` (key `(AXIS, USDC, 8200, 82, address(0))`). Log both.
    - `testWethRouteUnchanged`: v1's ticket-664 route under `LeanExecutor2` with Morpho grosses what v1's fork test does.
- [ ] **Step 2: Run them.**
  - Run: `FOUNDRY_PROFILE=fork forge test --match-path test/fork/LeanExecutor2Fork.t.sol -vv`.
  - Expected: PASS, with the two gas figures printed.
- [ ] **Step 3: Commit** the test file, with the gas figures in the message: `test(fork): LeanExecutor2 on AXIS v4/v3, v4 hop gas (Task 8.5 R37)`.

### Task 4: The deploy script, and authorizing the senders

**Files:**
- Create: `script/DeployLeanExecutor2.s.sol`, `test/lean/DeployLeanExecutor2.t.sol`
- Modify: `ops/host/ARMED.md` (a new "R37: the second executor" section)

**Interfaces:**
- Consumes: `LeanExecutor2`.
- Produces:
  - a dry-runnable deploy, `forge script script/DeployLeanExecutor2.s.sol --rpc-url base --sender 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3`;
  - the owner and recipient are `0x69D5…8A3`; the trader is `0xCB436Ba3acb945b3fc8EE6345857262584356595`;
  - the authorize step: `ops/armed-senders.json`'s `executor` set to the new address, then `AuthorizeArmedSenders.s.sol` as in ARMED.md §1c.

- [ ] **Step 1: Write the failing test.** `DeployLeanExecutor2.t.sol` mirrors `DeployLeanExecutor.t.sol`: the roles are set, `profitRecipient` is the owner's, and `USDC.allowance(executor, MORPHO_BLUE)` is the maximum.
- [ ] **Step 2: Run it to see it fail.** Run `forge test --match-path test/lean/DeployLeanExecutor2.t.sol`; expected a compile error.
- [ ] **Step 3: Implement the script, mirroring `DeployLeanExecutor.s.sol`, and write the ARMED.md section.** The section covers: dry run, the operator's broadcast, setting the address in `ops/armed-senders.json`, authorize, the boot check, and the revocation order.
- [ ] **Step 4: Run it to see it pass**, and run the script's dry run against `--rpc-url base` with nothing broadcast. Expected: a deployment simulated, the roles printed.
- [ ] **Step 5: Commit** the script, the test and `ops/host/ARMED.md`: `feat(script): deploy LeanExecutor2 (Task 8.5 R37)`.

## Phase 2: v4 pools in the book

### Task 5: `live::v4`, the pure pieces

**Files:**
- Create: `crates/apex-runtime/src/live/v4.rs`, `crates/apex-runtime/tests/live_v4.rs`
- Modify: `crates/apex-runtime/src/live/mod.rs`

**Interfaces:**
- Produces:
  - `pub const POOL_MANAGER: Address`
  - `pub const POOLS_SLOT: u64 = 6`
  - `pub fn key_address(pool_id: B256) -> Address`: the first 20 bytes.
  - `pub fn state_slot(pool_id: B256) -> B256`
  - `pub fn liquidity_slot(pool_id: B256) -> B256`: state + 3.
  - `pub fn bitmap_slot(pool_id: B256, word: i16) -> B256`: `keccak256(abi.encode(int16 word, state + 5))`.
  - `pub fn tick_slot(pool_id: B256, tick: i32) -> B256`: `keccak256(abi.encode(int24 tick, state + 4))`.
  - `pub struct Slot0 { pub sqrt_price_x96: U256, pub tick: i32, pub protocol_fee: u32, pub lp_fee: u32 }` and `pub fn decode_slot0(word: U256) -> Slot0`.
  - `pub fn hook_flags(hooks: Address) -> u16`: the low 14 bits.
  - `pub fn refusal(key: &V4Key, currency0: Address, currency1: Address, allowed_hooks: &BTreeSet<Address>) -> Option<&'static str>`. It returns why a pool is refused, or `None`.
  - `pub struct V4Key { pub pool_id: B256, pub fee: u32, pub tick_spacing: i32, pub hooks: Address }`. Task 6 adds it to `PoolSpec`.

- [ ] **Step 1: Write the failing tests** in `tests/live_v4.rs`:
  - `the_axis_pool_s_slots_are_the_chain_s`: `state_slot(0xc19d91…a627fa)` is `0xa446a125276052ec378b11eacacc4812f36f5e00ab66ee87cd802ad56ce21aa5`, as computed by `cast index bytes32 <id> 6`.
  - `slot0_decodes_as_state_view_reads_it`: the word `0x000000000064000000fb936200000000000000000000087d3eff65fbcecdc0a1` decodes to sqrtPriceX96 `40089314331252449198241`, tick −289950, protocol fee 0 and LP fee 100.
  - `v4_slots_sign_extend_negative_keys`: `bitmap_slot(id, -1)` equals keccak over a 32-byte `0xff…ff` key followed by state + 5, and `tick_slot(id, -289950)` likewise.
  - `the_alpha_hook_s_flags_are_before_initialize_add_liquidity_and_swap`: `hook_flags(0xb0b2…2880) == 0x2880`.
  - `refusals`, one assertion each:
    - native currency → `"native currency"`;
    - dynamic fee → `"dynamic fee"`;
    - a return-delta bit → `"hook returns a delta"`;
    - an unlisted hook → `"hook not allowlisted"`;
    - zero hooks → `None`.
- [ ] **Step 2: Run them to see them fail.** Run `cargo test -j 3 -p apex-runtime --test live_v4`; expected unresolved imports.
- [ ] **Step 3: Implement `live/v4.rs`.**
- [ ] **Step 4: Run them to see them pass.**
- [ ] **Step 5: Commit** `src/live/v4.rs`, `src/live/mod.rs` and `tests/live_v4.rs`: `feat(live): v4 slots, slot0, hook flags and admission rules (Task 8.5 R37)`.

### Task 6: `Venue::UniswapV4`, the inventory and the hook allowlist

**Files:**
- Modify:
  - `crates/apex-runtime/src/live/inventory.rs` (the variant, `PoolSpec.v4: Option<V4Key>`, and loading `uniswap_v4/pools.jsonl`);
  - the five `match`es on `Venue`: `live/pricing.rs`, `live/gas.rs`, `live/feed.rs`, `live/calls.rs`, `armed/predicate.rs`;
  - all 37 `PoolSpec {` literals, each with `v4: None` (the compiler lists them);
  - `crates/apex-runtime/src/shadow/config.rs` (`universe.v4_hooks: Option<PathBuf>`).
- Create: `ops/v4-hooks.base.json`, `crates/apex-runtime/tests/live_v4_inventory.rs`

**Interfaces:**
- Consumes: `V4Key`, `refusal`, `key_address` (Task 5).
- Produces:
  - `Venue::UniswapV4`:
    - `id()` is `venue_ids::UNISWAP_V4` (VenueId 8);
    - `dir()` is `"uniswap_v4"`;
    - `factory()` is `POOL_MANAGER`;
    - `fee_is_static()` is true, given admission;
    - `is_constant_product()` is false.
  - `inventory::load` reads v4 records of the form `{"pool_id","currency0","currency1","fee","tick_spacing","hooks","init_block","hub_usd_liquidity"?,"hub_symbol"?}`. A spec's `pool` is `key_address(pool_id)` and its `v4` is `Some(..)`.
  - Records that `refusal` rejects are dropped with a warning naming the pool and the reason. So is a key colliding with any other inventoried pool's address.
  - The hook file is `{"hooks":[{"address","code_hash","name","reason","reviewed"}]}`. Its first entry is `V4AlphaHook`:
    - address `0xb0b24b89db0dafbe43c5b40226b63a179f592880`;
    - code hash `0xdb992c225ee2523445c10a64cf01daa3bd6800d97abd94752be7ab2328aff659` (keccak of its code, 2026-10-28);
    - reason: "beforeSwap is a view that reverts until the pool has started; no delta, no fee override (verified source, 2026-10-28)".
  - `ops/shadow.base.yaml` and `ops/canary.base.yaml` gain `universe.v4_hooks: ops/v4-hooks.base.json`.

- [ ] **Step 1: Write the failing tests** in `tests/live_v4_inventory.rs`, against a temporary `data/base/uniswap_v4/pools.jsonl`:
  - `v4_records_load_under_their_key_address`
  - `an_unlisted_hook_and_a_dynamic_fee_are_dropped`
  - `a_key_colliding_with_an_inventoried_pool_is_dropped`
  - `the_shipped_hook_file_names_the_alpha_hook`: it parses `ops/v4-hooks.base.json`.
- [ ] **Step 2: Run them to see them fail.**
- [ ] **Step 3: Implement.** Adding `v4: None` across the literals is mechanical; `cargo build -j 3 --all-targets` lists each.
- [ ] **Step 4: Run them to see them pass**, plus `cargo test -j 3 -p apex-runtime --all-targets` (nothing else changes behaviour).
- [ ] **Step 5: Commit** every changed path, by name: `feat(live): Uniswap v4 venue, inventory and hook allowlist (Task 8.5 R37)`.

### Task 7: Reading v4 pools into the book

**Files:**
- Modify:
  - `crates/apex-runtime/src/live/reads.rs` (`extsload`);
  - `crates/apex-runtime/src/live/book.rs` (a v4 branch in `PoolBook::read`, and the protocol-fee refusal at every read);
  - `crates/apex-runtime/src/live/v4.rs` (`V4TickSource`).
- Create: `crates/apex-runtime/tests/live_v4_book.rs`

**Interfaces:**
- Consumes: Tasks 5 and 6.
- Produces:
  - `ChainReads::extsload(&self, slots: &[B256], block: u64) -> Result<Vec<U256>, ReadError>`: PoolManager's `extsload(bytes32[])`, selector `0xdbd035ff`, chunked at 256 slots a call.
  - `pub struct V4TickSource<'a> { reads: &'a ChainReads, ids: BTreeMap<Address, B256> }`, which implements `apex_venues::cl_ticks::TickDataSource` through `bitmap_slot` and `tick_slot`. A tick's word is `liquidityGross` (low 128) and `liquidityNet` (high 128).
  - In `PoolBook::read`, a v4 spec is read by `extsload` and gets a ladder through `build_ladder(&V4TickSource, key_address, …)`:
    - `factory` is `POOL_MANAGER`;
    - `code_hash` is the hook's (zero hooks: the empty-code hash);
    - `state.fee_ppm` is `lp_fee`.
  - It is refused with a new variant, `Unloadable::V4Refused { why: &'static str }`, when either holds:
    - `protocol_fee` is nonzero: `why = "protocol fee"`;
    - the hook's code hash differs from its allowlist entry's: `why = "hook code changed"`.

- [ ] **Step 1: Write the failing tests** in `tests/live_v4_book.rs`, with a scripted transport. It answers `eth_call` to PoolManager's `extsload(bytes32[])` from a slot map, and `aggregate3` as `tests/support` does.
  - `a_v4_pool_loads_its_state_and_ladder`: slot0, liquidity, one bitmap word and two ticks. The snapshot's sqrtPrice, tick, liquidity, fee and ladder ticks equal the inputs.
  - `a_v4_pool_whose_protocol_fee_turns_on_is_refused_at_read`: the same pool with protocolFee 0x00a00a is refused with `V4Refused { why: "protocol fee" }`.
  - `a_hook_whose_code_changed_is_refused`: the same pool with the hook's `eth_getCode` answering other bytes is refused with `V4Refused { why: "hook code changed" }`.
  - `negative_word_positions_read_their_own_slots`: the ladder below tick 0 reads the slots Task 5 computes for negative words.
- [ ] **Step 2: Run them to see them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them to see them pass.**
- [ ] **Step 5: Check against the chain once, without committing anything from it.**
  - Read the AXIS pool at a recent block through the new path, from a scratch test binary or `apex armed-replay`'s harness.
  - Compare against StateView's `getSlot0`, `getLiquidity`, `getTickBitmap(bytes32,int16)` (`0x1c7ccb4c`) and `getTickLiquidity(bytes32,int24)` (`0xcaedab54`), via `cast call` on PublicNode.
  - Expected: equal.
- [ ] **Step 6: Commit:** `feat(live): v4 pools read from PoolManager storage (Task 8.5 R37)`.

### Task 8: v4 quote exactness and the gas model

**Files:**
- Create:
  - `scripts/data/record_v4_fixture.py` (public endpoints, at one block: the pool's storage words, and `V4Quoter.quoteExactInputSingle` at `0x0d5e0F971ED27FBfF6c2837bf31316121532048D` for 12 sizes each way);
  - `crates/apex-runtime/tests/fixtures/v4_axis_quotes.json`;
  - `crates/apex-runtime/tests/live_v4_quote.rs`.
- Modify: `crates/apex-runtime/src/live/gas.rs` (`PerVenue.uniswap_v4`, set in `MEASURED` and `LEAN` from Task 3's figures).

**Interfaces:**
- Consumes: Task 7's snapshot construction (from storage words) and `LiveCycle`'s hop quote.
- Produces: `gas::LEAN.hop.uniswap_v4`, the hookless figure. A hooked pool adds the hook's figure minus the hookless one, as `PerVenue`-style data on `SettlementGas` (`v4_hook_extra: u64`).

- [ ] **Step 1: Record the fixture.**
  - Run: `python3 -I scripts/data/record_v4_fixture.py --pool 0xc19d91d9… --block <pinned> --out crates/apex-runtime/tests/fixtures/v4_axis_quotes.json`. The script verifies `0x0d5e…048D` is V4Quoter: it has code, and a quote for zero input reverts or returns zero.
  - Expected: 24 quotes written.
- [ ] **Step 2: Write the failing test.**
  - `every_recorded_quote_is_the_ladder_s_to_the_unit`: rebuild the snapshot from the recorded words, then assert each `quote_exact_input_multi_tick` output equals the quoter's, both directions.
  - `the_lean_gas_model_knows_v4`: `gas::LEAN.estimate` for a kind-3 hop is Task 3's figure.
- [ ] **Step 3: Run it to see it fail** (gas missing; quotes may also differ).
- [ ] **Step 4: Implement the gas entries.** If a quote differs, apply the step-for-step method from memory note `quote-exactness-is-step-for-step`: find the skipped step, never adjust the expectation.
- [ ] **Step 5: Run it to see it pass. Commit:** `test(live): v4 quotes exact against V4Quoter; v4 hop gas (Task 8.5 R37)`.

## Phase 3: the feed, and USDC

### Task 9: v4 events in the feed

**Files:**
- Modify:
  - `crates/apex-chain/src/rpc/ws.rs` (`LogFilter.topics1: Vec<B256>`; `filter_json` emits `[t0s]` or `[t0s, t1s]`);
  - `crates/apex-runtime/src/live/feed.rs` (`V4_SWAP`, `MODIFY_LIQUIDITY`, `decode_v4_swap`, `decode_modify_liquidity`, and the handler mapping them to writes keyed by `key_address`);
  - `crates/apex-runtime/src/shadow/mod.rs` (`subscriptions` takes `&[LogFilter]`; the feed adds the v4 filter when the book holds v4 pools).
- Test: `crates/apex-chain/tests/ws_filter.rs` (create if absent), `crates/apex-runtime/tests/live_v4_feed.rs`

**Interfaces:**
- Consumes: `key_address` (Task 5) and the book's v4 snapshots (Task 7).
- Produces:
  - `pub fn v4_filter(pool_ids: &[B256]) -> Option<LogFilter>`: `None` when the list is empty, so no unfiltered v4 subscription can exist;
  - `decode_v4_swap(&RawLog) -> Option<SwapLog>`: `pool = key_address(topic1)`, amounts negated to the pool's side, and the sqrtPrice, liquidity and tick;
  - `decode_modify_liquidity(&RawLog) -> Option<LiquidityLog>`: `None` when the `int256` delta doesn't fit `i128`.

- [ ] **Step 1: Write the failing tests:**
  - `a_filter_with_topic_one_lists_both_positions`, in `ws_filter.rs`;
  - `a_v4_swap_s_amounts_are_the_pool_s_side`: a Swap with amount0 = −1,000 (the swapper paid 1,000) decodes to `amount0 == 1000`;
  - `a_v4_swap_moves_its_pool_in_the_book`;
  - `a_modify_liquidity_becomes_a_position_write`;
  - `a_modify_liquidity_beyond_i128_reloads_the_pool`: the delta is 2^127, and the decode is `None` and the handler requests the pool's reload;
  - `no_pool_ids_no_v4_subscription`: `v4_filter(&[]) == None`.
- [ ] **Step 2: Run them to see them fail. Step 3: Implement. Step 4: Run them to see them pass.**
- [ ] **Step 5: Commit:** `feat(feed): v4 Swap and ModifyLiquidity, filtered by pool ID (Task 8.5 R37)`.

### Task 10: USDC as start and profit token

**Files:**
- Modify:
  - `crates/apex-runtime/src/live/frontier.rs` (`build_starts(chain, starts: &[Address], snapshot)`, merging each start's cycles; `Cycle.start` already says which);
  - `crates/apex-runtime/src/shadow/mod.rs` (the armed set from `build_starts(&[WETH, USDC])`; the reactive lane keeps WETH);
  - `crates/apex-runtime/src/armed/threshold.rs` (`Pricing.start_per_eth: Option<(U256, U256)>`, a rational of start units per wei);
  - `crates/apex-runtime/src/armed/engine.rs` (each cycle's costs and margin converted when `c.start != WETH`; the bid computed from the surplus converted back to wei);
  - `crates/apex-runtime/src/live/lenders.rs` (USDC holdings: Morpho Blue's `balanceOf`, and PoolManager's for routes with a v4 hop);
  - `crates/apex-runtime/src/shadow/config.rs` (`armed.max_size_usdc: u128`, default `12_000_000_000`, with an `ARMED_CAPS`-style ceiling of `100_000_000_000`);
  - `crates/apex-runtime/src/armed/live.rs` (the size cap by start token; `read_receipt` given the start token).
- Test: `crates/apex-runtime/tests/armed_usdc.rs`

**Interfaces:**
- Consumes: the book (Task 7).
- Produces:
  - `pub fn start_per_eth(snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>) -> Option<(U256, U256)>`, in `armed/threshold.rs`. It is the mid price of the WETH/USDC pool with the most liquidity among those the book holds, as USDC units per wei, kept as a sqrtPrice² ratio and never as a float.
  - `pub fn to_start(wei: U256, rate: (U256, U256)) -> U256`, rounded up for costs, and `pub fn to_wei(start: U256, rate) -> U256`, rounded down for surplus.

- [ ] **Step 1: Write the failing tests** in `tests/armed_usdc.rs`:
  - `a_usdc_arm_s_costs_are_converted_at_the_mid_price`: a WETH/USDC pool at tick −197,350 (about 2,688 USDC per WETH), and a cost of 1e15 wei.
    - `to_start` gives `(1e15 · sqrtP²).div_ceil(2^192)`, with `sqrtP = get_sqrt_ratio_at_tick(-197_350)`, computed in the test in `U256`.
    - It lies between 2,680,000 and 2,700,000 units ($2.68–2.70).
  - `a_surplus_converts_back_rounding_down`
  - `usdc_cycles_start_and_end_in_usdc`
  - `the_usdc_size_cap_holds`: an arm over 12,000 USDC is skipped as `size`.
  - `a_usdc_receipt_reads_its_profit_in_usdc`
- [ ] **Step 2: Run them to see them fail. Step 3: Implement. Step 4: Run them, and `cargo test -j 3 -p apex-runtime --all-targets`, to see them pass.**
- [ ] **Step 5: Commit:** `feat(armed): USDC start, costs converted at the book's mid price (Task 8.5 R37)`.

## Phase 4: targets, arming and evaluation

### Task 11: The census targets v4 and USDC routes

**Files:**
- Modify:
  - `scripts/data/arb_census/race.py` (`route_of` also returns `start_in`: the pool that took the sole gain token, when exactly one did; the record gains `"start_in"`);
  - `scripts/data/arb_census/targets.py` (`select` and `route_key`);
  - `crates/apex-runtime/src/armed/targets.rs`;
  - `scripts/data/arb_census/test_race.py`, `test_targets.py`.
- Test: the two Python files, and `crates/apex-runtime/tests/armed_targets.rs`.

**Interfaces:**
- Produces:
  - **`select` accepts:**
    - 2 hops;
    - venues within `race.OURS | {"uniswap_v4"}`;
    - a sole gain token of WETH or USDC;
    - offset 0;
    - a known `start_in`, falling back to `weth_in` for WETH routes in caches without `start_in`.
  - **A route's JSON** gains `"start": "<token>"`. Its `pools` hold 20-byte addresses for v2/v3 pools and 32-byte IDs for v4, start-in pool first.
  - **Rust:** `TargetRoute.start: Address`, defaulting to WETH when absent. Pools are parsed by `key_address` for 32-byte IDs, with `TargetRoute.v4_ids: Vec<B256>` kept.
  - `Targets::has(start_in, other)` is unchanged in shape.
- **Cache compatibility:** older race days have no `start_in`. Their USDC routes are skipped, not guessed, and the field fills in as days roll.

- [ ] **Step 1: Write the failing tests:**
  - `test_route_of_names_the_pool_that_took_the_gain_token`
  - `test_two_hop_v4_and_usdc_routes_are_selected`
  - `test_a_usdc_route_without_start_in_is_skipped`
  - `test_route_json_carries_start_and_v4_ids`
  - in Rust, `a_v4_route_parses_to_its_key_address_and_start`
- [ ] **Step 2: Run them to see them fail.** Run `python3 -m pytest scripts/data/arb_census -q` and `cargo test -j 3 -p apex-runtime --test armed_targets`.
- [ ] **Step 3: Implement. Step 4: Run them to see them pass. Step 5: Commit:** `feat(census): target v4 and USDC two-hop routes (Task 8.5 R37)`.

### Task 12: Predicates and storage checks on PoolManager

**Files:**
- Modify:
  - `crates/apex-runtime/src/armed/predicate.rs`: `layout(v: Venue)` becomes `layout_of(p: &PoolSnapshot) -> Layout`, where `Layout { address: Address, price_slot: Option<U256>, liquidity_slot: Option<U256>, reserve_slots: Option<(U256, U256)> }`. A v4 pool's address is PoolManager and its slots are Task 5's.
  - `crates/apex-runtime/src/armed/eval.rs`: `Write::Set.slot` and `Write::Liquidity.slot` become `U256`.
  - `crates/apex-runtime/src/live/reads.rs`: `storage_at(a, slot: U256, block)`.
  - `crates/apex-runtime/src/armed/engine.rs`: `verify_layouts` uses `layout_of`.
- Test: `crates/apex-runtime/tests/armed_predicate.rs`, `armed_run.rs`

**Interfaces:**
- Consumes: Tasks 5 and 7.
- Produces: `layout_of`. The existing callers move to it, and v3 pools' slots are unchanged.

- [ ] **Step 1: Write the failing tests:**
  - `a_v4_trigger_watches_pool_manager_storage`: the predicate's `address` is PoolManager, its `slot` is `0xa446a125…1aa5`, its mask is the price mask;
  - `a_v4_band_s_liquidity_word_is_state_plus_three`;
  - `verify_layouts_reads_v4_words_from_pool_manager`, with a scripted transport;
  - `a_v4_write_lands_on_its_256_bit_slot`, in `armed_run.rs`.
- [ ] **Step 2: Run them to see them fail. Step 3: Implement. Step 4: Run them, and `cargo test -j 3 -p apex-runtime --all-targets` (v3 predicates byte-identical), to see them pass.**
- [ ] **Step 5: Commit:** `feat(armed): v4 predicates on PoolManager storage (Task 8.5 R37)`.

### Task 13: Arms built for `LeanExecutor2`

**Files:**
- Modify:
  - `crates/apex-runtime/src/armed/plan.rs` (`armed_call2`, `armed_hops2`; `armed_gross` takes the start token and v4 hops);
  - `crates/apex-runtime/src/live/calls.rs` (`lean2_hop(leg, quote, min_out) -> Result<Lean2Hop, String>`, which fills `V4HopKey` from the spec's `v4`);
  - `crates/apex-runtime/src/shadow/config.rs` (`armed.executor2: Option<Executor2Config { address: Address, code_hash: B256, runtime_code: Option<PathBuf> }>`);
  - `crates/apex-runtime/src/armed/engine.rs` (arms built with `armed_call2` when `executor2` is set).
- Test: `crates/apex-runtime/tests/armed_predicate.rs` (plan tests).

**Interfaces:**
- Consumes: `apex_exec::lean2` (Task 1) and `V4Key` (Task 5).
- Produces:
  - `pub fn armed_call2(cycle: &Cycle, size: EthersU256, mid: EthersU256, output: EthersU256, executor: Address, chain_id: u64, deadline: u64, lender: Lean2Lender) -> Result<ExecutorCall, String>`.
  - The lender is `PoolManager` when either leg is v4. Otherwise the existing `Lenders` choice applies, mapped to `Lean2Lender`.
  - `runtime_code` is a path to the deployed bytecode's hex (`forge inspect LeanExecutor2 deployedBytecode`), used only for the pre-deploy simulation in Task 14.

- [ ] **Step 1: Write the failing tests:**
  - `an_armed_v4_route_borrows_from_the_pool_manager`
  - `an_armed_usdc_route_starts_and_ends_in_usdc`
  - `a_v4_hop_carries_its_pool_key`: fee, spacing and hooks from the spec;
  - `armed_gross_quotes_a_v4_hop_as_the_executor_runs_it`: the lean quote's rules apply unchanged; a v4 hop is exact-input like v3.
- [ ] **Step 2: Run them to see them fail. Step 3: Implement. Step 4: Run them to see them pass.**
- [ ] **Step 5: Commit:** `feat(armed): arms for LeanExecutor2, v4 hops and USDC (Task 8.5 R37)`.

### Task 14: Evaluating v4 and USDC fires before the deploy

**Files:**
- Modify: `crates/apex-runtime/src/armed/run.rs`, `crates/apex-runtime/src/armed/replay.rs`, `crates/apex-runtime/src/live/sim.rs` (if the override shape lives there)
- Test: `crates/apex-runtime/tests/armed_run.rs`

**Interfaces:**
- Consumes: Tasks 9, 12 and 13.
- Produces:
  - `get_logs` also reads PoolManager's Swap and ModifyLiquidity for the run's v4 pool IDs (topic 1).
  - `effects_of` maps them to `Write`s on `(POOL_MANAGER, slot)`.
  - `apply_through` applies v4 writes.
  - When `executor2.runtime_code` is set, `simulate` puts that code at `executor2.address` by state override. It sets `executors[trader]` by a storage override at `keccak256(abi.encode(trader, uint256(1)))`; `executors` is slot 1 after `owner`, and the builder verifies this with `forge inspect LeanExecutor2 storageLayout`.
  - USDC gross is valued in USD at 1:1 and in ETH by the run's WETH price.

- [ ] **Step 1: Write the failing tests:**
  - `v4_logs_become_writes_on_pool_manager_slots`
  - `a_v4_fire_s_book_applies_its_swaps_and_positions`
  - `the_pre_deploy_simulation_overrides_code_and_trader`: the request JSON carries `stateOverrides[executor].code` and the storage key.
  - `a_usdc_fire_s_gross_is_reported_in_usdc_and_usd`
- [ ] **Step 2: Run them to see them fail. Step 3: Implement. Step 4: Run them to see them pass.**
- [ ] **Step 5: Re-judge one recorded AXIS fire locally, as the R36 re-judge did.** Use a side directory and the BlockPI key from the environment only. Expected: the quote equals the simulation to the wei.
- [ ] **Step 6: Commit:** `feat(armed): evaluate v4 and USDC fires, simulated against LeanExecutor2 before its deploy (Task 8.5 R37)`.

### Task 15: Token admission by a simulated round trip

**Files:**
- Modify: `crates/apex-runtime/src/armed/run.rs` or `crates/apex-runtime/src/live/sim.rs` (where `simulate` builds its request), `crates/apex-runtime/src/shadow/mod.rs` (at boot, before arming)
- Test: `crates/apex-runtime/tests/armed_token_check.rs`

**Interfaces:**
- Consumes: the simulation request builder (Task 14).
- Produces: `pub async fn moves_exactly(rpc: &dyn RpcTransport, token: Address, holder: Address, block: u64) -> Result<bool, String>`.
  - One `eth_simulateV1` runs two `transfer` calls of 1,000 units, `holder` → a fresh address → `holder`. `holder` is a pool of the token, read from the book.
  - It is true when both transfers succeed and the fresh address's balance moves by exactly 1,000 each way (two `balanceOf` calls in the same simulation).
  - At boot, every targeted pool's non-start currency is checked. A token that fails is logged and its pools left unarmed, as `verify_layouts`' failures are. WETH and USDC are not checked.

- [ ] **Step 1: Write the failing tests:**
  - `a_plain_token_moves_exactly`
  - `a_fee_on_transfer_token_does_not`: the scripted simulation answers 990 received.
  - `a_reverting_transfer_does_not`
- [ ] **Step 2: Run them to see them fail. Step 3: Implement. Step 4: Run them to see them pass.**
- [ ] **Step 5: Commit:** `feat(armed): admit a v4 route's token only if it moves exactly (Task 8.5 R37)`.

### Task 16: The v4 inventory, kept daily, and the shadow configuration

**Files:**
- Create: `scripts/data/v4_inventory.py`, `scripts/data/test_v4_inventory.py`
- Modify:
  - `ops/host/apex-targets.service` (a daily scan before the census);
  - `ops/shadow.base.yaml` and `ops/canary.base.yaml` (`universe.v4_hooks`; `armed.executor2` with `runtime_code: var/armed/LeanExecutor2.runtime.hex` and a placeholder address until the deploy; `armed.max_size_usdc`);
  - `ops/host/ARMED.md` (gate A″, and the backfill procedure);
  - `PLAN.md` (an R37 entry).

**Interfaces:**
- Produces:
  - `v4_inventory.py --logs FILE` folds a file of `Initialize` logs, from any source, into `data/base/uniswap_v4/pools.jsonl`.
  - `--scan FROM TO --rpc URL` scans PoolManager's `Initialize` topic over a range in 2,000-block chunks.
  - Both are idempotent by pool ID and refuse a log not from PoolManager.
- **The one-time backfill is an agent step, recorded in ARMED.md:**
  1. Resolve the pool IDs in the census's race files through an explorer API.
  2. `get_logs` on PoolManager, topic0 `Initialize`, topic1 the ID, as done on 2026-10-28.
  3. Fold the results in with `--logs`.

- [ ] **Step 1: Write the failing tests:**
  - `test_initialize_logs_become_pool_records`: the AXIS pool's log from 2026-10-28 becomes `fee 100, tick_spacing 1, hooks 0xb0b2…`;
  - `test_a_second_fold_changes_nothing`;
  - `test_a_log_from_another_address_is_refused`.
- [ ] **Step 2: Run them to see them fail. Step 3: Implement. Step 4: Run them to see them pass.**
- [ ] **Step 5: Run all five gates.** Record the result.
- [ ] **Step 6: Commit** the named paths: `feat(ops): v4 inventory kept daily; shadow configured for LeanExecutor2 (Task 8.5 R37)`.

## Phase 5: shadow, then the pilot (operator steps, each approved at the time)

1. **Push and deploy in shadow:**
   - put the inventory backfill, the hook file and `LeanExecutor2.runtime.hex` on the host;
   - restart;
   - check that the boot reports v4 pools loaded, and `verify_layouts` clean for them.
2. **Gate A″, at least 3 days:**
   - quote and simulation agree on at least 95% of v4/USDC fires;
   - every armed v4 pool's storage matches the book;
   - would-have-won fires and net a day, against the actual winners.
3. **The operator's decisions** (spec decisions 2–4): sender funding, the live gate's threshold, budget netting.
4. **The owner deploy (Task 4's script), then authorize, then fund:**
   - set `armed.executor2.address` and `code_hash` to the deployed ones;
   - remove `runtime_code`;
   - restart.
5. **The pilot:** `armed.mode: live`, under R36's caps.
