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

**NOT GREEN.** Two pre-existing failures, recorded as finding B-13 in PLAN.md §3.4.
Task 0.2a fixes them and re-freezes this file; Phase 0 exits on green, not on "matches baseline".

```text
[FAIL: assertion failed: 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45 != 0x0000000000000000000000000000000000003001] testEnvAddressLookupAcceptsEthereumLongPrefixAlias() (gas: 18554)
[FAIL: MissingRequiredIntegration("BAL_VAULT|BALANCER_VAULT", 31337 [3.133e4], "OPT")] testDeployInitialisesExecutorOwnerToRouter() (gas: 7940404)
[FAIL: MissingRequiredIntegration("BAL_VAULT|BALANCER_VAULT", 31337 [3.133e4], "OPT")] testDeployTransfersRouterOwnershipToConfiguredExecutorOwner() (gas: 7943530)
Ran 12 test suites in 21.28ms (77.07ms CPU time): 63 tests passed, 3 failed, 0 skipped (66 total tests)
```

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
