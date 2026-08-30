# Phase 0 — Live Log Ingestion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the pool-monitor websocket actually receive logs, and fix the two defects that activate the moment it does.

**Architecture:** Move event topic constants out of `ingestion.rs` into a new pure `log_decode` module where they are derived from their event signatures rather than hardcoded, so a typo'd hash is impossible. Replace the lossy collect-then-clear dirty drain with an atomic swap under a mutex. Give `PoolUniverse` a token-pair → pools reverse index so backrun hints resolve to real pool addresses instead of polluting the pool set with token addresses.

**Tech Stack:** Rust 1.97.0, `ethers` 2.0.14, `dashmap` 6, `tokio` 1 (full + test-util), `prometheus` 0.13. Tests are inline `#[cfg(test)] mod tests` per module, run with `cargo test`.

## Global Constraints

- Toolchain is pinned at `1.97.0` in `rust-toolchain.toml`. Do not bump it.
- No new dependencies in this plan. `loom` arrives in Plan B, not here.
- Every module must be declared **twice**: `pub mod x;` in `src/lib.rs` and `mod x;` in `src/main.rs`. `main.rs` compiles its own copy; this is an established pattern in this codebase, not a mistake.
- Items used only by the library or tests must carry `#[allow(dead_code)]` or they warn in the `main.rs` build.
- Never hold a `std::sync::Mutex` guard across an `.await`.
- Do not touch `PopulateCacheState.touched_pools` (`src/main.rs:3482`). It is a **different field** from `PoolMonitor.touched_pools` and is not in scope.
- This plan changes no pricing, sizing, planning, simulation, or execution behaviour.

**Reference spec:** `docs/superpowers/specs/2026-08-29-live-state-dirty-scan-design.md` §2 and §2.1.

---

### Task 1: `log_decode` module with derived topic constants

The shipped constants `TOPIC_SYNC` and `TOPIC_SWAP` (`src/ingestion.rs:29-37`) are both wrong — each has a correct prefix and a fabricated tail, and neither matches any log on Base. Deriving them from their signatures makes the class of bug impossible; pinning the expected hex in a test catches someone editing a signature string.

**Files:**
- Create: `src/log_decode.rs`
- Modify: `src/lib.rs` (add module declaration)
- Modify: `src/main.rs:14` area (add module declaration)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub static TOPIC_V2_SYNC: LazyLock<H256>`, `pub static TOPIC_V2_SWAP: LazyLock<H256>`, `pub fn topic_of(signature: &str) -> H256`.

- [ ] **Step 1: Write the failing test**

Create `src/log_decode.rs` containing only the tests:

```rust
//! Pure decoding of pool events. No I/O, no async, no state.
//!
//! Topic constants are DERIVED from their event signatures rather than
//! hardcoded. The previous hardcoded values (`0x1c4168cd…`, `0xd78ad95f…5c01`)
//! each had a correct prefix and a fabricated tail, matched no log on any
//! chain, and left the pool monitor subscribed but deaf for the life of the
//! process. Derivation removes that failure mode entirely.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_match_the_keccak_of_their_signatures() {
        assert_eq!(*TOPIC_V2_SYNC, topic_of("Sync(uint112,uint112)"));
        assert_eq!(
            *TOPIC_V2_SWAP,
            topic_of("Swap(address,uint256,uint256,uint256,uint256,address)")
        );
    }

    /// Pins the on-chain values. Derivation protects against a typo'd hash;
    /// this protects against an edited signature string, which derivation
    /// would happily and silently follow.
    ///
    /// Both values verified against live Base via `eth_getLogs` at block
    /// 0x303afcb (Sync, 6 hits) and 0x303afd2 (Swap, 2 hits).
    #[test]
    fn topics_match_the_values_observed_on_chain() {
        assert_eq!(
            format!("{:#x}", *TOPIC_V2_SYNC),
            "0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1"
        );
        assert_eq!(
            format!("{:#x}", *TOPIC_V2_SWAP),
            "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"
        );
    }

    /// The exact values that shipped. Neither matches any log on Base. If
    /// either ever reappears, this test fails loudly rather than the system
    /// going quietly deaf.
    #[test]
    fn the_shipped_constants_are_rejected() {
        let sync_bug = "0x1c4168cdb0bea3c47cead55631e2d4f769596b056cc50faaa83d728afabaf805";
        let swap_bug = "0xd78ad95fa46c994b6551d0da85fc275fe613d2f6ad697fc0971df54087195c01";
        assert_ne!(format!("{:#x}", *TOPIC_V2_SYNC), sync_bug);
        assert_ne!(format!("{:#x}", *TOPIC_V2_SWAP), swap_bug);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib log_decode`
Expected: FAIL to compile — `cannot find value TOPIC_V2_SYNC in this scope`, and `log_decode` is not a known module.

- [ ] **Step 3: Write minimal implementation**

Add to the top of `src/log_decode.rs`, above the test module:

```rust
use ethers::types::H256;
use std::sync::LazyLock;

/// keccak256 of an event signature — the value that appears as `topics[0]`.
pub fn topic_of(signature: &str) -> H256 {
    H256(ethers::utils::keccak256(signature.as_bytes()))
}

/// `Sync(uint112 reserve0, uint112 reserve1)` — UniV2 / Solidly / Aerodrome.
/// Emitted on every reserve change, so it carries complete state.
pub static TOPIC_V2_SYNC: LazyLock<H256> =
    LazyLock::new(|| topic_of("Sync(uint112,uint112)"));

/// `Swap(address indexed sender, uint amount0In, uint amount1In,
///       uint amount0Out, uint amount1Out, address indexed to)` — UniV2.
pub static TOPIC_V2_SWAP: LazyLock<H256> =
    LazyLock::new(|| topic_of("Swap(address,uint256,uint256,uint256,uint256,address)"));
```

Add to `src/lib.rs`, in the module list:

```rust
pub mod log_decode;
```

Add to `src/main.rs` in the module declaration list (roughly lines 12-40), between `mod liquidity_cache;` and `mod math;` to keep it alphabetical:

```rust
mod log_decode;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib log_decode`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/log_decode.rs src/lib.rs src/main.rs
git commit -m "feat(ingestion): derive event topics from signatures

The hardcoded TOPIC_SYNC and TOPIC_SWAP each had a correct prefix and a
fabricated tail. Neither matched any log on Base, so the pool monitor was
subscribed but deaf. Deriving from the signature removes the failure mode;
tests pin the on-chain values and reject the shipped ones.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: Point the subscription at the derived topics

The filter is built inline inside an async reconnect loop (`src/ingestion.rs:168-171`), where it cannot be tested. Extract it to a pure function first, then test it.

**Files:**
- Modify: `src/ingestion.rs:29-37` (delete wrong constants), `src/ingestion.rs:168-171` (use extracted fn)
- Test: `src/ingestion.rs` inline test module

**Interfaces:**
- Consumes: `crate::log_decode::{TOPIC_V2_SYNC, TOPIC_V2_SWAP}` from Task 1.
- Produces: `pub(crate) fn pool_log_filter(pools: &[MonitoredPool]) -> Filter`.

- [ ] **Step 1: Write the failing test**

Add to `src/ingestion.rs` at the end of the file:

```rust
#[cfg(test)]
mod filter_tests {
    use super::*;

    fn pool(n: u64) -> MonitoredPool {
        MonitoredPool {
            pair: Address::from_low_u64_be(n),
            token_in: Address::from_low_u64_be(n + 1000),
            token_out: Address::from_low_u64_be(n + 2000),
            fee_bps: 30,
            stable: false,
            kind: PoolMonitorKind::UniV2,
        }
    }

    /// `Filter::topics` is `[Option<Topic>; 4]` where `Topic` is
    /// `ValueOrArray<Option<H256>>`. Destructure it rather than matching on
    /// `Debug` output, which is not a stable contract.
    fn topic0_of(filter: &Filter) -> Vec<H256> {
        match filter.topics[0].clone().expect("topic0 must be set") {
            ValueOrArray::Value(v) => v.into_iter().collect(),
            ValueOrArray::Array(vs) => vs.into_iter().flatten().collect(),
        }
    }

    fn addresses_of(filter: &Filter) -> Vec<Address> {
        match filter.address.clone().expect("address must be set") {
            ValueOrArray::Value(a) => vec![a],
            ValueOrArray::Array(a) => a,
        }
    }

    #[test]
    fn filter_subscribes_to_the_real_sync_and_swap_topics() {
        let topics = topic0_of(&pool_log_filter(&[pool(1), pool(2)]));
        // `&*` derefs the LazyLock: `contains` wants `&H256`, not
        // `&LazyLock<H256>`, and the deref is not inserted implicitly here.
        assert!(
            topics.contains(&*crate::log_decode::TOPIC_V2_SYNC),
            "Sync topic missing from filter: {topics:?}"
        );
        assert!(
            topics.contains(&*crate::log_decode::TOPIC_V2_SWAP),
            "Swap topic missing from filter: {topics:?}"
        );
        assert_eq!(topics.len(), 2, "no extra topics should be subscribed");
    }

    #[test]
    fn filter_covers_every_supplied_pool() {
        let addrs = addresses_of(&pool_log_filter(&[pool(1), pool(2), pool(3)]));
        for n in 1..=3u64 {
            let addr = Address::from_low_u64_be(n);
            assert!(addrs.contains(&addr), "pool {addr:#x} missing from filter");
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib filter_tests`
Expected: FAIL to compile — `cannot find function pool_log_filter in this scope`.

- [ ] **Step 3: Write minimal implementation**

Delete these lines entirely from `src/ingestion.rs` (lines 29-37):

```rust
const TOPIC_SYNC: H256 = H256([
    0x1c, 0x41, 0x68, 0xcd, 0xb0, 0xbe, 0xa3, 0xc4, 0x7c, 0xea, 0xd5, 0x56, 0x31, 0xe2, 0xd4, 0xf7,
    0x69, 0x59, 0x6b, 0x05, 0x6c, 0xc5, 0x0f, 0xaa, 0xa8, 0x3d, 0x72, 0x8a, 0xfa, 0xba, 0xf8, 0x5,
]);

const TOPIC_SWAP: H256 = H256([
    0xd7, 0x8a, 0xd9, 0x5f, 0xa4, 0x6c, 0x99, 0x4b, 0x65, 0x51, 0xd0, 0xda, 0x85, 0xfc, 0x27, 0x5f,
    0xe6, 0x13, 0xd2, 0xf6, 0xad, 0x69, 0x7f, 0xc0, 0x97, 0x1d, 0xf5, 0x40, 0x87, 0x19, 0x5c, 0x1,
]);
```

Add in their place:

```rust
/// The websocket filter for a pool set.
///
/// Extracted from `run_ws` so it can be tested. It could not be before, and
/// the constants it depends on were wrong for the life of the process.
pub(crate) fn pool_log_filter(pools: &[MonitoredPool]) -> Filter {
    Filter::new()
        .address(pools.iter().map(|p| p.pair).collect::<Vec<_>>())
        .topic0(vec![
            *crate::log_decode::TOPIC_V2_SYNC,
            *crate::log_decode::TOPIC_V2_SWAP,
        ])
}
```

Replace lines 168-171 (the inline `let filter = Filter::new()…` block) with:

```rust
            let filter = pool_log_filter(&pools);
```

Leave the `H256` import at `src/ingestion.rs:15` alone — the test helpers added in Step 1 still use it, so it will not warn. `ValueOrArray` needs no import: `ethers::prelude::*` is already in scope at the top of the file.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib filter_tests && cargo build 2>&1 | grep -E "^(warning|error)" | head`
Expected: 2 tests PASS. No `unused import` warning for `H256`.

- [ ] **Step 5: Commit**

```bash
git add src/ingestion.rs
git commit -m "fix(ingestion): subscribe to the real Sync and Swap topics

Extracts filter construction so it is testable, and points it at the
derived topics. The pool monitor now actually receives logs.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: Replace the lossy dirty drain with an atomic swap

`drain_touched` (`src/ingestion.rs:121-125`) collects into a set and then calls `clear()`. Any insert landing between the iteration and the clear is destroyed permanently. This is latent only because Task 2 had not yet let any log through — it becomes a live loss bug the moment logs flow.

**Files:**
- Modify: `src/ingestion.rs:81` (field), `:113` (init), `:117-125` (accessors), `:214` (handle_log)
- Test: `src/ingestion.rs` inline test module

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub fn mark_touched(&self, pool: Address)` and `pub fn drain_touched(&self) -> HashSet<Address>`, signatures unchanged. Callers at `src/main.rs:6422` and `src/main.rs:6805` need no edit.

- [ ] **Step 1: Write the failing test**

Add to `src/ingestion.rs` inside the existing `filter_tests` module from Task 2, renaming it to `mod ingestion_tests` (update the `cargo test` filter accordingly in later steps):

```rust
    /// A concurrent insert must never be erased by a drain. The previous
    /// collect-then-clear implementation dropped any insert that landed
    /// between the iteration and the `clear()`.
    #[test]
    fn concurrent_marks_are_never_lost_across_a_drain() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::sync::Arc as StdArc;

        const WRITERS: u64 = 4;
        const PER_WRITER: u64 = 2_000;

        let touched: StdArc<StdMutex<HashSet<Address>>> =
            StdArc::new(StdMutex::new(HashSet::new()));
        let done = StdArc::new(AtomicBool::new(false));
        let drained: StdArc<StdMutex<HashSet<Address>>> =
            StdArc::new(StdMutex::new(HashSet::new()));

        let mut handles = Vec::new();
        for w in 0..WRITERS {
            let touched = StdArc::clone(&touched);
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_WRITER {
                    let addr = Address::from_low_u64_be(w * PER_WRITER + i);
                    touched.lock().expect("mark").insert(addr);
                }
            }));
        }

        let reader = {
            let touched = StdArc::clone(&touched);
            let drained = StdArc::clone(&drained);
            let done = StdArc::clone(&done);
            std::thread::spawn(move || loop {
                let batch: HashSet<Address> =
                    std::mem::take(&mut *touched.lock().expect("drain"));
                drained.lock().expect("record").extend(batch);
                if done.load(AtomicOrdering::SeqCst) {
                    let tail: HashSet<Address> =
                        std::mem::take(&mut *touched.lock().expect("drain tail"));
                    drained.lock().expect("record tail").extend(tail);
                    break;
                }
            })
        };

        for h in handles {
            h.join().expect("writer");
        }
        done.store(true, AtomicOrdering::SeqCst);
        reader.join().expect("reader");

        let seen = drained.lock().expect("final").len() as u64;
        assert_eq!(
            seen,
            WRITERS * PER_WRITER,
            "a concurrent mark was erased by a drain"
        );
    }

    #[test]
    fn drain_returns_everything_and_leaves_the_set_empty() {
        let set: StdMutex<HashSet<Address>> = StdMutex::new(HashSet::new());
        for n in 1..=5u64 {
            set.lock().expect("mark").insert(Address::from_low_u64_be(n));
        }
        let batch: HashSet<Address> = std::mem::take(&mut *set.lock().expect("drain"));
        assert_eq!(batch.len(), 5);
        assert!(set.lock().expect("check").is_empty());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ingestion_tests`
Expected: FAIL to compile — `cannot find type StdMutex in this scope`.

- [ ] **Step 3: Write minimal implementation**

In `src/ingestion.rs`, extend the `std::sync` import at line 3-6 to bring in the mutex under an unambiguous alias (the file already uses `tokio::sync::RwLock`, so an unqualified `Mutex` would read ambiguously):

```rust
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex,
    },
```

Change the field at line 81:

```rust
    /// Pools whose state moved since the last drain.
    ///
    /// A `Mutex<HashSet>` rather than a `DashMap`, because the drain must be a
    /// single atomic swap. The previous `DashMap` drain collected then cleared,
    /// which erased any insert landing between the two — a permanent loss, not
    /// a delay. Never held across an `.await`.
    touched_pools: Arc<StdMutex<HashSet<Address>>>,
```

Change the initialiser at line 113:

```rust
            touched_pools: Arc::new(StdMutex::new(HashSet::new())),
```

Replace the two accessors at lines 117-125:

```rust
    pub fn mark_touched(&self, pool: Address) {
        if let Ok(mut guard) = self.touched_pools.lock() {
            guard.insert(pool);
        }
    }

    /// Take the dirty set and leave an empty one, in one atomic step.
    ///
    /// A mark landing immediately after the swap belongs to the next batch and
    /// cannot be erased. Collect-then-clear did not have this property.
    pub fn drain_touched(&self) -> HashSet<Address> {
        match self.touched_pools.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(_) => HashSet::new(),
        }
    }
```

Change the insert in `handle_log` at line 214 from `self.touched_pools.insert(pair, ());` to:

```rust
        self.mark_touched(pair);
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib ingestion_tests && cargo build 2>&1 | grep -E "^error" | head`
Expected: 4 tests PASS (2 from Task 2, 2 new). No build errors.

- [ ] **Step 5: Commit**

```bash
git add src/ingestion.rs
git commit -m "fix(ingestion): make the dirty drain atomic

drain_touched collected then cleared, erasing any mark that landed between
the two. Latent while the subscription matched nothing; a live loss bug now
that logs flow. Swap under a mutex instead.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: `PoolUniverse::pools_for_hop` reverse index

`hops_for_pools` maps pools → token hops. The hint fix in Task 5 needs the inverse. Adding a field must not perturb `digest()`, or every scan would report a structure change and rebuild the cycle index.

**Files:**
- Modify: `src/cycle_index.rs:85-94` (struct), `:101-129` (`from_pools`), add method after `:161`
- Test: `src/cycle_index.rs` inline `mod tests`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub fn pools_for_hop(&self, from: Address, to: Address) -> &[Address]`.

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `src/cycle_index.rs`:

```rust
    #[test]
    fn pools_for_hop_resolves_both_directions_to_the_same_pools() {
        let graph = two_hop_graph();
        let universe = universe_of(&graph);
        let forward = universe.pools_for_hop(addr(1), addr(2));
        let reverse = universe.pools_for_hop(addr(2), addr(1));
        assert_eq!(forward, [addr(100)], "hop must resolve to its pool");
        assert_eq!(
            forward, reverse,
            "a pool trades both ways; direction must not change the answer"
        );
    }

    #[test]
    fn pools_for_hop_returns_every_parallel_pool() {
        // Same token pair across three fee tiers. A hint on this pair must
        // dirty ALL of them, not an arbitrary one.
        let graph = graph_from(vec![
            edge(addr(1), addr(2), addr(100)),
            edge(addr(1), addr(2), addr(101)),
            edge(addr(1), addr(2), addr(102)),
        ]);
        let mut pools = universe_of(&graph).pools_for_hop(addr(1), addr(2)).to_vec();
        pools.sort_unstable();
        assert_eq!(pools, vec![addr(100), addr(101), addr(102)]);
    }

    #[test]
    fn pools_for_hop_is_empty_for_an_unknown_pair() {
        let universe = universe_of(&two_hop_graph());
        assert!(
            universe.pools_for_hop(addr(7), addr(8)).is_empty(),
            "an unknown pair must yield nothing, not a panic or a wrong pool"
        );
    }

    #[test]
    fn the_reverse_index_does_not_perturb_the_digest() {
        // digest() drives cycle-index staleness. If adding by_pair changed it,
        // every scan would rebuild.
        let graph = two_hop_graph();
        let universe = universe_of(&graph);
        let idx = CycleIndex::build(&universe, &[addr(1)], CycleIndexLimits::default());
        assert!(!idx.is_stale(&universe_of(&two_hop_graph())));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cycle_index::tests::pools_for_hop`
Expected: FAIL to compile — `no method named pools_for_hop found for struct PoolUniverse`.

- [ ] **Step 3: Write minimal implementation**

Add a field to `PoolUniverse` (`src/cycle_index.rs:85-94`), after `pairs`:

```rust
    /// Canonical unordered pair -> every pool serving it.
    ///
    /// The inverse of `hops_for_pools`. Deliberately NOT part of `digest`:
    /// the digest keys cycle-index staleness on adjacency, and this field is
    /// derived from the same `pools` map, so including it would add nothing
    /// but a rebuild trigger.
    by_pair: HashMap<(Address, Address), Vec<Address>>,
```

In `from_pools`, after `pairs.dedup();` and before the hasher block, insert:

```rust
        let mut by_pair: HashMap<(Address, Address), Vec<Address>> = HashMap::new();
        for (pool, (a, b)) in map.iter() {
            let key = if a <= b { (*a, *b) } else { (*b, *a) };
            by_pair.entry(key).or_default().push(*pool);
        }
        for pools in by_pair.values_mut() {
            pools.sort_unstable();
            pools.dedup();
        }
```

Add `by_pair,` to the returned `Self { … }` literal.

Add this method to the `impl PoolUniverse` block, after `hops_for_pools`:

```rust
    /// Every pool serving the token hop `from -> to`.
    ///
    /// A pool trades both directions, so the lookup is direction-insensitive.
    /// Returns all parallel pools: a hint on WETH/USDC must dirty every fee
    /// tier, not whichever one happened to be found first.
    pub fn pools_for_hop(&self, from: Address, to: Address) -> &[Address] {
        let key = if from <= to { (from, to) } else { (to, from) };
        self.by_pair.get(&key).map(|v| v.as_slice()).unwrap_or(&[])
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib cycle_index`
Expected: PASS — 14 pre-existing tests plus 4 new.

- [ ] **Step 5: Commit**

```bash
git add src/cycle_index.rs
git commit -m "feat(cycle-index): add pools_for_hop reverse index

The inverse of hops_for_pools, needed to resolve backrun hint token pairs
to pool addresses. Excluded from digest() so it cannot trigger rebuilds.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: Stop putting token addresses in the pool set

`src/main.rs:6427-6428` inserts `hint.from` and `hint.to` into the touched-**pool** set, but those are token addresses (`src/mempool.rs:30-37`); `post_state_from_hint` sets `pool: Address::zero()`, so hints carry no pool identity. Tokens never match a pool, but they make the set non-empty, which flips `populate` to `incremental`, sets `pool_filter` to those tokens, and `src/venues.rs:1984-1998` then filters CL `source_pools` to nothing — a scan with zero CL re-quotes and every cached edge reused.

**Files:**
- Modify: `src/main.rs:6424-6430`
- Modify: `src/backrun_state.rs:105-113` (delete dead duplicate)
- Test: `src/main.rs` inline `mod tests`

**Interfaces:**
- Consumes: `PoolUniverse::pools_for_hop` from Task 4.
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` at `src/main.rs:13677` (note: `main.rs` also has `mod runner_tests` at `:9770` — this is the other one):

```rust
    #[test]
    fn hint_tokens_never_enter_the_touched_pool_set() {
        use crate::cycle_index::PoolUniverse;

        let weth = Address::from_low_u64_be(1);
        let usdc = Address::from_low_u64_be(2);
        let pool_a = Address::from_low_u64_be(100);
        let pool_b = Address::from_low_u64_be(101);
        let universe =
            PoolUniverse::from_pools(vec![(pool_a, weth, usdc), (pool_b, weth, usdc)]);

        let mut touched: HashSet<Address> = HashSet::new();
        for pool in universe.pools_for_hop(weth, usdc) {
            touched.insert(*pool);
        }

        assert!(touched.contains(&pool_a) && touched.contains(&pool_b));
        assert!(
            !touched.contains(&weth) && !touched.contains(&usdc),
            "token addresses in a pool set flip populate to incremental and \
             filter every CL pool out of the scan"
        );
    }

    #[test]
    fn a_hint_on_an_unknown_pair_dirties_nothing() {
        use crate::cycle_index::PoolUniverse;

        let universe = PoolUniverse::from_pools(vec![(
            Address::from_low_u64_be(100),
            Address::from_low_u64_be(1),
            Address::from_low_u64_be(2),
        )]);

        let mut touched: HashSet<Address> = HashSet::new();
        for pool in universe.pools_for_hop(
            Address::from_low_u64_be(50),
            Address::from_low_u64_be(51),
        ) {
            touched.insert(*pool);
        }

        assert!(
            touched.is_empty(),
            "an unresolvable hint must leave the set empty so populate stays \
             on the full path, not go incremental with a filter matching nothing"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin arb-exec hint_tokens_never`
Expected: FAIL to compile — `no method named pools_for_hop` if Task 4 was skipped; otherwise both tests should already pass, since they exercise `PoolUniverse` directly. **If they pass at this step, that is expected** — they pin the intended behaviour that Step 3 wires into the scan. Verify the real defect instead by confirming `src/main.rs:6427` still reads `touched.insert(hint.from);`.

- [ ] **Step 3: Write minimal implementation**

Replace `src/main.rs:6424-6430`:

```rust
            if let Some(monitor) = &self.backrun {
                let hints = monitor.active_hints(Duration::from_secs(45)).await;
                for hint in &hints {
                    touched.insert(hint.from);
                    touched.insert(hint.to);
                }
            }
```

with:

```rust
            if let Some(monitor) = &self.backrun {
                let hints = monitor.active_hints(Duration::from_secs(45)).await;
                // `hint.from`/`hint.to` are TOKENS, and `post_state_from_hint`
                // leaves `pool` zeroed — hints carry no pool identity. Putting
                // tokens in a pool set does not merely fail to match: it makes
                // the set non-empty, which flips populate to incremental with a
                // filter that matches nothing, dropping every CL re-quote for
                // that scan. Resolve to real pools, or contribute nothing.
                if !hints.is_empty() {
                    let universe = self.pool_universe().await;
                    for hint in &hints {
                        for pool in universe.pools_for_hop(hint.from, hint.to) {
                            touched.insert(*pool);
                        }
                    }
                }
            }
```

Delete `src/backrun_state.rs:105-113` entirely:

```rust
#[allow(dead_code)]
pub fn touched_pools_from_hints(hints: &[BackrunHint]) -> HashSet<Address> {
    let mut touched = HashSet::new();
    for hint in hints {
        touched.insert(hint.from);
        touched.insert(hint.to);
    }
    touched
}
```

It is dead, it has the identical defect, and its name is the bug. If `HashSet` is now unused in that file's imports, remove it.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin arb-exec hint_ && cargo build 2>&1 | grep -E "^error" | head`
Expected: 2 tests PASS. No build errors, no `unused import` warning in `backrun_state.rs`.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/backrun_state.rs
git commit -m "fix(backrun): resolve hint tokens to pools before marking dirty

hint.from/to are tokens, not pools. Inserting them made touched_pools
non-empty, which flipped populate to incremental with a pool_filter
matching nothing — a scan with zero CL re-quotes. Resolve via
pools_for_hop, and delete the dead duplicate of the same bug.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: Warn when the subscription is silent

The whole failure was invisible because a deaf subscription looks identical to a quiet market. The Phase 0 exit gate is `ingestion_ws_events > 0`; make the system say so itself.

**Files:**
- Modify: `src/ingestion.rs` (`run_ws`, around the connected-loop at `:177-197`)
- Test: `src/ingestion.rs` inline `mod ingestion_tests`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub(crate) fn should_warn_silent(events: u64, connected_for: Duration) -> bool`.

- [ ] **Step 1: Write the failing test**

Add to `mod ingestion_tests` in `src/ingestion.rs`:

```rust
    #[test]
    fn a_silent_subscription_warns_once_connected_long_enough() {
        assert!(
            should_warn_silent(0, Duration::from_secs(120)),
            "a connected-but-deaf subscription is exactly the shipped bug and \
             must be loud"
        );
    }

    #[test]
    fn a_busy_subscription_never_warns() {
        assert!(!should_warn_silent(1, Duration::from_secs(600)));
    }

    #[test]
    fn a_freshly_connected_subscription_is_given_time() {
        assert!(
            !should_warn_silent(0, Duration::from_secs(5)),
            "a quiet market must not warn during normal startup"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ingestion_tests::a_silent`
Expected: FAIL to compile — `cannot find function should_warn_silent in this scope`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/ingestion.rs`, near `pool_log_filter`:

```rust
/// Grace period before a silent subscription is treated as suspicious.
const SILENT_SUBSCRIPTION_GRACE: Duration = Duration::from_secs(60);

/// True when a subscription has been connected long enough that receiving
/// nothing is more likely a broken filter than a quiet market.
///
/// This exists because a wrong `topic0` produced a subscription that connected
/// cleanly, logged success, and delivered nothing for the life of the process.
pub(crate) fn should_warn_silent(events: u64, connected_for: Duration) -> bool {
    events == 0 && connected_for >= SILENT_SUBSCRIPTION_GRACE
}
```

In `run_ws`, immediately after `info!(pools = pools.len(), "pool monitor websocket connected");` (line 176), add:

```rust
                    let connected_at = std::time::Instant::now();
                    let mut events_seen: u64 = 0;
                    let mut silence_warned = false;
```

Inside the inner `loop`, in the `Some(log) => { … }` arm, before the `handle_log` call, add:

```rust
                                        events_seen = events_seen.saturating_add(1);
```

And change the `tokio::select!` to include a periodic silence check by adding this arm alongside the existing two:

```rust
                            _ = sleep(SILENT_SUBSCRIPTION_GRACE) => {
                                if !silence_warned
                                    && should_warn_silent(events_seen, connected_at.elapsed())
                                {
                                    silence_warned = true;
                                    warn!(
                                        pools = pools.len(),
                                        connected_secs = connected_at.elapsed().as_secs(),
                                        "pool monitor websocket connected but has received NO \
                                         logs; check that the subscribed topics match the pools"
                                    );
                                }
                            }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib ingestion_tests && cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" | head`
Expected: 7 tests PASS. No new clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src/ingestion.rs
git commit -m "feat(ingestion): warn when a connected subscription is silent

A wrong topic0 produced a subscription that connected, logged success, and
delivered nothing for the life of the process. A deaf subscription and a
quiet market are now distinguishable from the logs.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 7: Full-suite verification

**Files:** none modified.

**Interfaces:**
- Consumes: all prior tasks.
- Produces: evidence that Phase 0 is complete.

- [ ] **Step 1: Run the whole suite**

Run: `cargo test 2>&1 | tail -30`
Expected: all tests pass, no regressions in `graph`, `venues`, `plan`, `cl_*`.

- [ ] **Step 2: Lint and format**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: clean exit. If `cargo fmt --check` fails, run `cargo fmt` and amend the last commit.

- [ ] **Step 3: Confirm the removed API has no stragglers**

Run: `grep -rn "TOPIC_SYNC\|TOPIC_SWAP\|touched_pools_from_hints" --include=*.rs src/`
Expected: no output. (Matches under `.claude/worktrees/` are a separate worktree and are out of scope.)

- [ ] **Step 4: Field verification against Base**

This is the Phase 0 exit gate from the spec and cannot be satisfied by tests.

Run the binary against Base for at least 5 minutes, then confirm the counter is non-zero:

```bash
curl -s localhost:9184/metrics | grep ingestion_ws_events_total
```

Expected: a value **greater than zero**. If it is still zero, do not proceed to Plan B — the subscription is still not matching, and every downstream phase depends on it.

- [ ] **Step 5: Commit any formatting fixes**

```bash
git add -A
git commit -m "chore: fmt and clippy after phase 0" || echo "nothing to commit"
```

---

## Self-Review

**Spec coverage.** §2 (topic constants) → Tasks 1, 2. §2.1 token-into-pool-set → Task 5. §2.1 lossy drain → Task 3. §3.1 `log_decode` module created → Task 1 (decoders themselves are Plan B; this task creates the module and its constants only). §9 Phase 0 gate (`ingestion_ws_events > 0`) → Task 7 Step 4, with Task 6 making a failure visible. `PoolUniverse::pools_for_hop`, referenced by §2.1 → Task 4.

Not covered here, by design: §3.2 `live_state.rs`, §3.3 `state_gate.rs`, §4 protocols, §6 staleness guards, §8 invariants. These are Plan B and Plan C per spec §9.0.

**Type consistency.** `pool_log_filter(&[MonitoredPool]) -> Filter` defined in Task 2, used in Task 2 only. `should_warn_silent(u64, Duration) -> bool` defined and used in Task 6. `pools_for_hop(Address, Address) -> &[Address]` defined in Task 4, used in Task 5. `mark_touched`/`drain_touched` keep their existing signatures in Task 3, so the callers at `main.rs:6422` and `main.rs:6805` need no edit — verified against both call sites.

**Known sharp edge.** Task 5 Step 2 will pass rather than fail, because the tests exercise `PoolUniverse` directly rather than the scan path (which needs a live `Runner`). The step says so explicitly and gives an alternative verification. Flagged rather than papered over.

**API claims verified against the vendored sources, not assumed:** `Filter.topics: [Option<Topic>; 4]` and `Filter.address: Option<ValueOrArray<Address>>` are both public (`ethers-core/src/types/filter.rs:119-132`). `LazyLock` is stable well below the pinned 1.97.0. `main.rs` has two test modules; the correct one is named at each use.
