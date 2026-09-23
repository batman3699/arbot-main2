# APEX-MEV v4 — pre-migration baseline

Captured 2026-09-22T03:51:52Z at commit `728791e` on branch `phase0-workspace-split`.
Every Phase 0 acceptance check diffs against this file. Re-freeze it after Task 0.2a.

## cargo check --all-targets
```text
exit 0
1 warning: src/venues.rs:633 `edge_capacity_from_cl_state` is never used
```

## cargo test --all-targets

NOTE: the tree compiles a DUAL module tree — `lib.rs` declares 42 modules,
`main.rs` declares 58, and the shared files are compiled twice. That is why
there are two large suites below. Record BOTH; `tail` would truncate one.

```text
test result: ok. 589 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.13s
test result: ok. 776 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 1.20s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
```

TOTAL: 1374 passed, 0 failed, 2 ignored, across 9 targets.

## forge test

> **RESOLVED 2026-09-24 by Phase 5 Task 5.7.** Everything below was true at the
> Phase 0 freeze and is kept as recorded — a baseline that gets edited when the
> world changes is not a baseline. What changed: `script/Deploy.s.sol` grew a
> `DeployConfig` seam, so `configFromEnv` is the only function in the Solidity
> tree that reads the environment and `resolveConfig`/`runWith` are functions of
> their argument. The three deploy test files now pass explicit structs or
> per-test synthetic keys, and no key is written by two files —
> `scripts/ci/no_shared_env_keys.sh` gates both halves of that.
>
> **Re-measured, whole suite, no `--no-match-path`:**
>
> ```text
> run 1   107/0     run 6   107/0
> run 2   107/0     run 7   107/0
> run 3   107/0     run 8   107/0
> run 4   107/0     run 9   107/0
> run 5   107/0     run 10  107/0
> ```
>
> 107 rather than 67 because Phases 1–5 added 40 tests. The CI exclusion is
> removed; `forge test` gates unqualified. See PLAN.md Task 5.7 for the two
> deliberate deviations from its own Step 1 sketch — in particular, the sketch's
> hermetic test perturbed `CHAIN`, which would have re-created this defect
> across files while claiming to close it.

**NOT GREEN, AND NOT DETERMINISTIC.** This is the corrected finding; see B-13 in
PLAN.md §3.4. The first freeze recorded "64 passed, 2 failed" as though it were a
fixed pair. It is not — the suite is **flaky**, which is worse than red, because a
flaky gate cannot certify anything.

Measured over 8 consecutive unmodified `forge test` runs:

```text
run 1   67/0      run 5   67/0
run 2   65/2      run 6   67/0
run 3   67/0      run 7   67/0
run 4   66/1      run 8   66/1
```

The failing set varies run to run and has included at least:
`testEnvAddressLookupAcceptsEthereumLongPrefixAlias`,
`testDeployTransfersRouterOwnershipToConfiguredExecutorOwner`,
`testEnvAddressLookupUsesMappedOptimismPrefix`,
`testDefaultDoesNotRequireBalancerOrAave`.

**Root cause.** The deploy tests configure themselves with `vm.setEnv`, which
writes the *forge process* environment — global, shared, and persisting for the
whole run. forge additionally auto-loads `.env`, which supplies `CHAIN=base`,
`ETH_UNIV3_ROUTER` and `BASE_EXECUTOR_OWNER`; the prefixed keys correctly outrank
the unprefixed ones the tests set. So each test's result depends on the ambient
`.env` AND on what every other test wrote before it. `script/Deploy.s.sol`'s
precedence logic is correct — this is a test-architecture defect.

**Scope.** Entirely confined to the 15 `test/Deploy*.t.sol` tests. The other 52
are clean: `forge test --no-match-path 'test/Deploy*'` gives 52/0.

**What was tried and did NOT work** (recorded so it is not retried):

| Attempt | Result |
|---|---|
| Synthetic probe keys `.env` cannot define | Each test passes ALONE; no improvement in-file |
| Pinning `CHAIN` + setting both key forms | Ownership test passes ALONE; no improvement in-file |
| `threads = 1` in foundry.toml (confirmed read by `forge config`) | Still 6/12 runs failing — the tests share one process either way |
| One forge process per `Deploy*` file | Reduces but does not remove it; intra-file pollution remains |
| Combined, measured per-file | 11 passed/4 failed vs 11/3 for the originals — **no measurable gain, so reverted** |

There is no `unsetEnv` cheatcode in forge 1.7, so a test cannot isolate itself
from `.env` or from its siblings. The real fix is to stop configuring the deploy
script through the process environment — which is the same global-mutable-state
pathology INV-11 forbids in the engine, appearing here in the test harness.

**Decision required before Phase 0 can exit** — see the Phase 0 exit gate.

## Size

```text
rust    67 files, 73194 LOC
solidity 23 files, 2144 LOC
src/main.rs 16659 LOC (was 13,039 when the 2026 audit flagged it as "large, not started")
ARBOT_* env vars: 84 distinct
```

## ARBOT_* environment variables (Task 0.5 must account for every one)

```text
ARBOT_ARB_FEE_CEILING_PPM
ARBOT_BASE_FAST
ARBOT_BASE_FAST_ALLOW_MARGIN_HOPS
ARBOT_BASE_FAST_MAX_CYCLES
ARBOT_BASE_FAST_MAX_INDEX_CYCLES
ARBOT_BASE_FAST_PREPARE
ARBOT_BASE_FAST_PREP_SLOTS
ARBOT_BASE_FAST_RECONCILE_SECS
ARBOT_BASE_FAST_REF_NATIVE
ARBOT_BASE_FAST_VERIFY_BATCH
ARBOT_BASE_FAST_VERIFY_TTL_SECS
ARBOT_BASE_RPC_HTTP
ARBOT_BF_SKIP_ON_STABLE_GRAPH
ARBOT_BID_PROFIT_FRACTION_BPS
ARBOT_BLOCK_POLL_MS
ARBOT_CANDIDATE_CONCURRENCY
ARBOT_CENSUS
ARBOT_CENSUS_PATH
ARBOT_CENSUS_PER_HOPS
ARBOT_CL_EXEC_BUFFER_BPS
ARBOT_CL_LADDER_WORDS
ARBOT_CL_MAX_TICKS
ARBOT_CL_MULTI_TICK
ARBOT_CL_PARITY_CHECKS_PER_SCAN
ARBOT_CL_PARITY_MAX_ERR_BPS
ARBOT_CL_PARITY_TTL_SECS
ARBOT_CL_QUOTE_PARITY
ARBOT_CL_TICK_BUFFER_BPS
ARBOT_COST_COMPETITION_BPS
ARBOT_COST_EXEC_BUFFER_BPS
ARBOT_COST_FLASH_FEE_BPS
ARBOT_COST_GAS_BPS
ARBOT_COST_RISK_BPS
ARBOT_CYCLE_INDEX_COMPARE
ARBOT_DEPTH_DIVISOR
ARBOT_DISABLE_PRIVATE_RAW_FALLBACK
ARBOT_DUMP_CALLDATA
ARBOT_DUMP_CALLDATA_PATH
ARBOT_ENABLE_JIT
ARBOT_ENV
ARBOT_FILTER_UNFUNDABLE
ARBOT_FORCE_ATTEMPT
ARBOT_FORK_RPC_URL
ARBOT_GAS_RESERVE_TXS
ARBOT_HUB_SEARCH
ARBOT_HUB_SEARCH_PARALLEL_EDGES
ARBOT_INTEGRATION_CHAIN
ARBOT_INTEGRATION_SMOKE
ARBOT_INTERACTIVE
ARBOT_L2_SIM_CEILING_MS
ARBOT_LIVE_STATE_SHADOW
ARBOT_LOCAL_CL_QUOTES
ARBOT_MAX_CYCLE_FEE_BPS
ARBOT_NEW_BLOCK_WAIT_MS
ARBOT_NONINTERACTIVE
ARBOT_RELAY_PARALLEL_BLAST
ARBOT_REQUIRE_CHAIN_COVERAGE
ARBOT_RESCAN_SAME_BLOCK
ARBOT_RPC_QUOTE_TIMEOUT_SECS
ARBOT_RPC_URL
ARBOT_SCAN_IDLE_SLEEP_MS
ARBOT_SIM_CASCADE_DEPTH
ARBOT_SIM_L1_FEE
ARBOT_SIM_PREFETCH
ARBOT_SIM_PREFETCH_MAX_ACCOUNTS
ARBOT_SIM_QUORUM_MODE
ARBOT_SIM_QUORUM_TIMEOUT_MS
ARBOT_SIM_REVM
ARBOT_SIM_REVM_EXECUTOR_BYTECODE
ARBOT_SIM_REVM_LIVE
ARBOT_SIM_REVM_TIMEOUT_MS
ARBOT_SLIPSTREAM_LIVE_QUOTE
ARBOT_SMOKE_EXECUTOR
ARBOT_START_TOKENS
ARBOT_STATE_GATE_CHECKS_PER_SCAN
ARBOT_STATE_GATE_MAX_ERR_BPS
ARBOT_STATE_GATE_TTL_SECS
ARBOT_STATE_VALIDATION_SECS
ARBOT_TEST_ENV_FLAG_9
ARBOT_TEST_ENV_PARSE_9
ARBOT_TEST_ENV_U256_9
ARBOT_TIP_BPS
ARBOT_UNIV3_QUEUE_WAIT_TIMEOUT_SECS
ARBOT_UNIV3_TOTAL_DEADLINE_SECS
```

## Contract size, measured 2026-09-23 (first CI run)

`MultiVenueArbImplementation` compiles to **24,403 bytes** of runtime code.
EIP-170's limit is 24,576. **173 bytes of margin — 99.3% of the ceiling.**

This is a hard constraint on Phase 5, which replaces `_execGeneric` and folds
`BatchRouter`'s ownership into `contracts/core/ExecutionAuth.sol`. Any net
addition to the implementation fails to deploy. Phase 5 has to remove before it
adds, and `_execGeneric`'s deletion is what buys the room.

It is also why `forge build --sizes` failed on the first CI run:
`JitRemoveHarness` (test/MultiVenueArbExecutor.t.sol) extends the
implementation and adds scaffolding, so it lands at 25,790 — 1,214 over. Every
subclass of a contract with 173 bytes left is over by construction. Nothing
deploys that harness, and JIT is Retired in the migration matrix (§1.4), so the
harness dies with the feature.

`scripts/ci/check_executor_size.sh` now asks the question that matters: every
**deployable** contract, meaning everything not declared under `test/` or
`contracts/mocks/`, against EIP-170. It prints the margin for anything within
1 KiB of the ceiling, so 24,403 is visible in every run rather than discovered
the next time someone adds a function.

### Resolved 2026-09-24 — Phase 5 Task 5.5

Deleting the JIT and bridge paths freed **5,980 bytes**. The implementation is
now **18,423 bytes, 6,153 to spare — 75.0% of EIP-170**, against Phase 5's
benchmark of 15% margin. The 173-byte cliff above is history, and the room
Task 5.1's `AdapterRegistry` substitution needs is there.

`JitRemoveHarness` went with the feature, so `forge build --sizes` would now
pass on its own. The CI build step stays as plain `forge build` regardless: any
future test harness that extends the executor will put `--sizes` back over the
limit for the same non-reason, and `check_executor_size.sh` asks the question
that actually matters.
