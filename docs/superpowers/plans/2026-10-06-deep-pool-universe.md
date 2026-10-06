# Deep-Pool Universe Implementation Plan (Task 8.5 R20)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Admit Base's deepest WETH/USDC and WETH/cbBTC pools on the three reachable venues into the shadow's universe, moving it from 4 pairs, 12 pools and 30 cycles to about 8, 24 and 74.

**Architecture:** The universe filter (fee ceiling, depth floor) moves from a code default into a `universe:` section of the shadow config, set to 3,000 ppm and $100k. A committed script records the on-chain fee of every Slipstream inventory record that lacks one, because the loader skips those rather than guess. A throwaway probe proves the book quotes the new pools exactly before the restart.

**Tech Stack:** Rust (apex-runtime, serde_yaml config), Python 3 (data script, JSON-RPC over urllib), BlockPI (Base RPC).

**Spec:** `docs/superpowers/specs/2026-10-06-deep-pool-universe-design.md`

## Global Constraints

- Never print, echo or log secret values (`BLOCKPI_KEY`, `PRIVATE_KEY`, `TRADER_PRIVATE_KEY`, `APEX_SECRET_*`). Pass the RPC URL through the environment (`BASE_RPC_URL`), never argv.
- Python requests to BlockPI need a `User-Agent` header, or BlockPI answers 403.
- Never run `cargo fmt`. Use `-j 3` for cargo: this machine has 15 GB, and builds have OOM-killed other processes.
- Never `git add -A`: stage named paths, then check `git diff --stat -- <paths>` is empty.
- Run all five CI gates before committing (scratchpad `gates.sh`). `check_placeholder_endpoints.sh` and `cl_parity_sweep.sh` are known-red.
- Verify each commit in a clean worktree with its own `CARGO_TARGET_DIR`.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- `UniverseFilter::default()` stays 500 ppm and $100k, the census's numbers. Only the shadow config moves.
- The Slipstream loader rule stays: a record without `fee_ppm_onchain` is skipped, never guessed.

---

### Task 1: The universe filter is a shadow setting

**Files:**
- Modify: `crates/apex-runtime/src/shadow/config.rs` (new `UniverseConfig`; `Raw` and `ShadowConfig` gain `universe`; validation)
- Modify: `crates/apex-runtime/src/shadow/mod.rs:214-224` (`universe()` takes a filter) and `:349` (boot passes `config.universe.filter()`)
- Modify: `ops/shadow.base.yaml` (new `universe:` section)
- Test: `crates/apex-runtime/tests/shadow_parts.rs`

**Interfaces:**
- Produces: `apex_runtime::shadow::config::UniverseConfig { pub max_fee_ppm: u32, pub min_depth_usd: u64 }`, with `fn filter(&self) -> UniverseFilter`. `ShadowConfig.universe: UniverseConfig`. `pub fn universe(dir: &Path, venues: &[Venue], filter: UniverseFilter) -> Result<Vec<PoolSpec>, InventoryError>`.

- [ ] **Step 1: Write the failing tests** in `crates/apex-runtime/tests/shadow_parts.rs`.

Add to the `yaml()` helper's text, after the `capacity:` block:

```yaml
universe:
  max_fee_ppm: 3000
  min_depth_usd: 100000
```

Add to `the_shipped_config_parses`:

```rust
    // R20: the shadow prices the deep pools the census's 500 ppm left out.
    assert_eq!(c.universe, UniverseConfig { max_fee_ppm: 3_000, min_depth_usd: 100_000 });
```

Add these tests:

```rust
/// **The run sets its own universe filter (R20).** The shadow's config names
/// the fee ceiling and depth floor; the census's 500 ppm stays the default.
#[test]
fn the_universe_filter_is_the_runs_to_set() {
    let var = "APEX_SECRET_SHADOW_TEST_UNI";
    let c = ShadowConfig::from_yaml_str(&yaml(var, "var/apex/shadow.journal"), &env(var, SECRET)).unwrap();
    assert_eq!(c.universe.filter(), UniverseFilter { max_fee_ppm: 3_000, min_depth_usd: 100_000.0 });

    // A ceiling of zero admits no pool: refused, not run empty.
    let text = yaml(var, "var/apex/shadow.journal").replace("max_fee_ppm: 3000", "max_fee_ppm: 0");
    assert!(matches!(ShadowConfig::from_yaml_str(&text, &env(var, SECRET)), Err(ShadowConfigError::Invalid(_))));
}

/// **The fee ceiling decides which pools pair.** At the census's 500 ppm a
/// 3,000 ppm pool is out and its pair keeps two pools; at 3,000 it is in. A
/// pool under the depth floor is out at any ceiling.
#[test]
fn the_universe_follows_its_filter() {
    use apex_runtime::live::inventory::Venue;
    let dir = tempfile::tempdir().unwrap();
    let weth = "0x4200000000000000000000000000000000000006";
    let usdc = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    let rec = |pool: u8, fee: u32, depth: f64| {
        serde_json::json!({ "pool": format!("0x{:040x}", pool), "token0": weth, "token1": usdc,
                            "fee": fee, "fee_ppm_onchain": fee, "hub_usd_liquidity": depth }).to_string()
    };
    std::fs::create_dir_all(dir.path().join("uniswap_v3")).unwrap();
    let lines = [rec(1, 500, 5e6), rec(2, 3_000, 60e6), rec(3, 100, 1e5), rec(4, 3_000, 5e4)];
    std::fs::write(dir.path().join("uniswap_v3").join("pools.jsonl"), lines.join("\n")).unwrap();
    for v in ["aerodrome_slipstream", "pancakeswap_v3"] {
        std::fs::create_dir_all(dir.path().join(v)).unwrap();
        std::fs::write(dir.path().join(v).join("pools.jsonl"), "").unwrap();
    }
    let at = |filter| {
        let mut got: Vec<u8> = apex_runtime::shadow::universe(dir.path(), &[Venue::UniswapV3], filter)
            .unwrap()
            .iter()
            .map(|s| s.pool.as_slice()[19])
            .collect();
        got.sort();
        got
    };
    assert_eq!(at(UniverseFilter::default()), vec![1, 3]);
    assert_eq!(at(UniverseFilter { max_fee_ppm: 3_000, min_depth_usd: 100_000.0 }), vec![1, 2, 3]);
}
```

Change the existing `the_universe_is_the_reachable_weth_pairs_with_two_pools` call to `apex_runtime::shadow::universe(dir.path(), venues, UniverseFilter::default())`. Add the imports `use apex_runtime::shadow::config::UniverseConfig;` and `use apex_runtime::live::inventory::UniverseFilter;`.

- [ ] **Step 2: Run them to see them fail**

Run: `CARGO_TARGET_DIR=target cargo test -j 3 -p apex-runtime --test shadow_parts`
Expected: compile errors (`UniverseConfig` missing, `universe` takes 2 arguments). Add stubs so it compiles: the struct with the two fields, `filter()` returning `UniverseFilter::default()`, `universe()` with an ignored third parameter, and `universe` in `Raw`/`ShadowConfig`. Then run again.
Expected: FAIL in `the_universe_filter_is_the_runs_to_set` (500 vs 3,000), `the_universe_follows_its_filter` (`[1, 3]` vs `[1, 2, 3]`), and `the_shipped_config_parses` (missing field `universe`).

- [ ] **Step 3: Implement**

In `config.rs`, next to `Policy`:

```rust
/// Which pools the run may price, by the inventory's measured fee and depth
/// (R20). The census's 500 ppm left out Base's deepest WETH/USDC and
/// WETH/cbBTC pools, where the large swaps behind every real gap land.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UniverseConfig {
    /// The highest fee a pool may charge, in ppm.
    pub max_fee_ppm: u32,
    /// The least a pool must hold, in whole US dollars.
    pub min_depth_usd: u64,
}

impl UniverseConfig {
    pub fn filter(&self) -> UniverseFilter {
        UniverseFilter { max_fee_ppm: self.max_fee_ppm, min_depth_usd: self.min_depth_usd as f64 }
    }
}
```

Add `universe: UniverseConfig` to `Raw` and to `ShadowConfig` (with the doc comment "Which pools the run prices."), set `universe: raw.universe` in `from_yaml_str`, and validate before `Ok(Self { .. })`:

```rust
        if raw.universe.max_fee_ppm == 0 {
            return Err(invalid("a universe fee ceiling of zero admits no pool"));
        }
```

Import `crate::live::inventory::UniverseFilter`.

In `shadow/mod.rs`:

```rust
pub fn universe(dir: &Path, venues: &[Venue], filter: UniverseFilter) -> Result<Vec<PoolSpec>, inventory::InventoryError> {
    let specs: Vec<PoolSpec> = inventory::load(dir, filter)?
```

At boot: `let specs = universe(&config.inventory, &venues, config.universe.filter()).map_err(|e| boot_err("the inventory", e))?;`

In `ops/shadow.base.yaml`, after `capacity:`:

```yaml
# Which pools the run may price (R20, operator decision 2026-10-06). The
# census's 500 ppm left out Base's deepest WETH/USDC and WETH/cbBTC pools,
# Uniswap's 0.3% ($60M, $6.3M) and Slipstream's spacing-100 ($8.8M at ~570
# ppm, $17M at 2,500), where the large swaps behind every real gap land.
# Pricing is exact at any fee; a 30 bps pool simply pays less often.
universe:
  max_fee_ppm: 3000
  min_depth_usd: 100000
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `CARGO_TARGET_DIR=target cargo test -j 3 -p apex-runtime --test shadow_parts`
Expected: all pass. Then `cargo test -j 3 -p apex-runtime --all-targets` (all pass) and `cargo clippy -j 3 -p apex-runtime --all-targets -- -D warnings` (clean).

- [ ] **Step 5: Mutation check**

Two mutants in `config.rs`, each restored and the file touched afterwards: `filter()` returning `UniverseFilter::default()`, and the zero-ceiling check removed. Two in `shadow/mod.rs`: `universe()` loading with `UniverseFilter::default()`. Each must fail `shadow_parts`. The boot line passing `config.universe.filter()` is checked by review.

---

### Task 2: Record the Slipstream fees the inventory lacks

**Files:**
- Create: `scripts/data/measure_slipstream_fees.py`
- Modify (data, outside git): `data/base/aerodrome_slipstream/pools.jsonl`

**Interfaces:**
- Produces: `fee_ppm_onchain` (int, ppm) and `fee_measured_block` (int) on every Slipstream record that lacked a measured fee and whose `fee()` reads. The Rust loader reads `fee_ppm_onchain` and ignores unknown fields.

- [ ] **Step 1: Write the script**

```python
#!/usr/bin/env python3
"""measure_slipstream_fees.py -- record the fee of every Slipstream pool the
inventory has no measured fee for.

The shadow's loader skips a Slipstream record without `fee_ppm_onchain`
rather than guess one: its `fee` field is the tick spacing, and reading that
as a fee would admit a 200-spacing pool as a 200 ppm one. So 29 WETH pools of
$100k or more were never priced, Base's deepest WETH/USDC and WETH/cbBTC
Slipstream pools among them (R20, 2026-10-06). This reads each such pool's
fee() at one pinned block and writes it, with the block, into the record.

A Slipstream fee is dynamic: the reading is one moment's, and decides only
whether the pool is admitted. The book prices every swap at the fee the
module charges then.

The RPC endpoint comes from BASE_RPC_URL (or PROBE_RPC) and is never printed.
The inventory is backed up before it is rewritten. Re-running measures
nothing new.

Usage: BASE_RPC_URL=... scripts/data/measure_slipstream_fees.py [--dry-run]
"""
import argparse, json, os, shutil, sys, time, urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
INVENTORY = REPO / "data" / "base" / "aerodrome_slipstream" / "pools.jsonl"
FEE = "0xddca3f43"  # fee()


def rpc_url() -> str:
    for key in ("BASE_RPC_URL", "PROBE_RPC"):
        if os.environ.get(key, "").strip():
            return os.environ[key].strip()
    sys.exit("set BASE_RPC_URL")


def call(url: str, method: str, params: list):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json", "User-Agent": "measure-slipstream-fees/1"})
    with urllib.request.urlopen(req, timeout=30) as r:
        out = json.loads(r.read())
    if "error" in out:
        raise RuntimeError(out["error"].get("message", "rpc error"))
    return out["result"]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true", help="report the fees, write nothing")
    args = ap.parse_args()
    url = rpc_url()
    records = [json.loads(l) for l in INVENTORY.read_text().splitlines() if l.strip()]
    block = int(call(url, "eth_blockNumber", []), 16)
    measured, unreadable = 0, []
    for r in records:
        if r.get("fee_ppm_onchain") is not None:
            continue
        try:
            fee = int(call(url, "eth_call", [{"to": r["pool"], "data": FEE}, hex(block)]), 16)
        except (RuntimeError, ValueError) as e:
            unreadable.append((r["pool"], str(e)))
            continue
        print(f"{r['pool']} spacing {r.get('fee')} fee {fee} ppm")
        r["fee_ppm_onchain"], r["fee_measured_block"] = fee, block
        measured += 1
    print(f"{measured} measured at block {block}; {len(unreadable)} unreadable")
    for pool, why in unreadable:
        print(f"  unreadable {pool}: {why}")
    if args.dry_run or measured == 0:
        return 0
    backup = INVENTORY.with_name(f"pools.jsonl.bak.fees-{int(time.time())}")
    shutil.copy2(INVENTORY, backup)
    INVENTORY.write_text("".join(json.dumps(r) + "\n" for r in records))
    print(f"written; the previous inventory is {backup.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
```

- [ ] **Step 2: Dry run**

Run (the key reaches the script through the environment, never argv):
`K=$(grep '^BLOCKPI_KEY=' .env | cut -d= -f2- | tr -d '"'"'"' ') BASE_RPC_URL="https://base.blockpi.network/v1/rpc/$K" python3 scripts/data/measure_slipstream_fees.py --dry-run; unset K`
Expected: every Slipstream record lacking a fee listed with a fee, including `0xb2cc…` (~570 ppm) and `0x70ac…` (~2,500 ppm), and `0 unreadable`. Nothing written.

- [ ] **Step 3: Measure for real, and check it is idempotent**

Run the same command without `--dry-run`. Expected: `written; the previous inventory is pools.jsonl.bak.fees-<ts>`. Run it again: `0 measured`.

- [ ] **Step 4: Confirm the universe it gives**

With the loader's rules at 3,000 ppm and $100k (the scratchpad `universe.py`, or a short inline equivalent), expect 8 WETH pairs, 24 pools and 74 two-hop cycles. That includes WETH/USDC with 7 pools and WETH/cbBTC with 5.

- [ ] **Step 5: Commit the script**

```bash
git add scripts/data/measure_slipstream_fees.py
git commit -m "feat(data): record the Slipstream fees the inventory lacks (Task 8.5 R20)"
```

The message body says why (29 deep WETH pools skipped), that the data lives outside git, and ends with the Co-Authored-By line.

---

### Task 3: Prove the book quotes the new pools exactly

**Files:**
- Create (scratchpad, not committed): `<scratchpad>/quote_parity/Cargo.toml`, `<scratchpad>/quote_parity/src/main.rs`

**Interfaces:**
- Consumes: `apex_runtime::live::book::PoolBook::load(&ChainReads, &[PoolSpec], block) -> Result<(PoolBook, Vec<Unloaded>), ReadError>`; `apex_runtime::live::reads::ChainReads::new(Arc<dyn RpcTransport>)`; `apex_runtime::shadow::universe(dir, venues, UniverseFilter)`; `apex_runtime::live::pricing::MAX_TICKS` (64); `apex_math::cl_swap::quote_exact_input_multi_tick(&ClPoolState, &TickLadder, U256 /* ethers */, bool, u32) -> Option<MultiTickQuote>`.

- [ ] **Step 1: Write the probe.** For each pool in `universe(data/base, &Venue::ALL, {3000, 100k})` that is not in today's universe (`{500, 100k}`):
  1. Find a recent block in which the pool swapped: `eth_getLogs` over the last 3,000 blocks for its Swap topic (`0xc42079f9…` for Uniswap and Slipstream). Use the newest log's block, so a Slipstream fee is the after-first fee the book prices.
  2. Load the book for that one pool at that block: `PoolBook::load(&reads, &[spec], block)`.
  3. For token0→token1 and token1→token0, at 0.01, 0.1 and 1 WETH (for the non-WETH side, the token amount that 0.01, 0.1 and 1 WETH quote to), compute `quote_exact_input_multi_tick(&pool.state, &pool.ladder, amount, zero_for_one, MAX_TICKS)`. Call the venue's quoter at the same block: Uniswap QuoterV2 `0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a`, selector `0xc6a5026a` with (tokenIn, tokenOut, amountIn, fee, 0); Slipstream quoter `0x254cF9E1E6e233aa1AC962CB9B05b2cfeAaE15b0`, selector `0x9e7defe6` with (tokenIn, tokenOut, amountIn, tickSpacing, 0).
  4. Print pool, direction, size, book out, quoter out, the difference in units and in bps, and ticks crossed.

  RPC from `PROBE_HTTP` (environment), as the earlier probes did.

- [ ] **Step 2: Build and run it**

`CARGO_TARGET_DIR=<scratchpad>/probe-target cargo build -j 3 --release`, then run with `PROBE_HTTP` set from `.env` the same way as Task 2.
Expected: every quote agrees to the wei, or within a few units on a multi-tick quote (R4 saw 2 units in 2.6 × 10⁹). A larger gap is a defect: stop and debug it before Task 4, using the memory note "Quote exactness is step for step" and replaying with `eth_simulateV1`.

---

### Task 4: Roll it out

**Files:**
- Modify: `PLAN.md` (R20 entry after R19)
- Memory: `shadow-run-operations.md`, `MEMORY.md`

- [ ] **Step 1: PLAN.md R20 entry.** Cover what the spec found, the decision, the measured fees, the parity results from Task 3 and the mutant count.
- [ ] **Step 2: All five CI gates.** Run the scratchpad `gates.sh`. Expected: only the two known-red scripts fail, clippy is ok, and every test passes.
- [ ] **Step 3: Commit Task 1 and the PLAN entry** by named paths (`crates/apex-runtime/src/shadow/config.rs`, `crates/apex-runtime/src/shadow/mod.rs`, `crates/apex-runtime/tests/shadow_parts.rs`, `ops/shadow.base.yaml`, `PLAN.md`). Check `git diff --stat -- <paths>` is empty, then verify in a clean worktree with its own `CARGO_TARGET_DIR`: `cargo test -j 3 --locked -p apex-runtime --all-targets`.
- [ ] **Step 4: Release build and restart.** Run `CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo build -j 3 --release -p apex-runtime --bin apex`. SIGINT the running shadow by its pid and wait with `timeout 90 tail --pid`. Archive the log, report and misses to `var/apex/postdeploy-13/`, copying the journal. Start with `setsid nohup scripts/shadow.sh > var/apex/shadow.log 2>&1 < /dev/null &`.
- [ ] **Step 5: Confirm the boot and the first report.** The boot line should show `pools` about 23 held of `universe` about 24 (the dead bsdETH pool unreadable), `cycles` about 74, and no new "not loaded" or "not admitted" warnings. The first five-minute report should show `unpriced 0`, no read failures, and a Tier 2 block named. Re-arm the unusual-event watch.
- [ ] **Step 6: Memory.** Record the new 14-day start and the universe size in `shadow-run-operations.md` and `MEMORY.md`. Report to the operator where the 14 days now count from.
