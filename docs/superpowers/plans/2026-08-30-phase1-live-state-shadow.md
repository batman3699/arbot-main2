# Phase 1 — Live State (Shadow Mode) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Maintain live pool state in memory from decoded logs, and prove it agrees with the chain — without any of it reaching pricing.

**Architecture:** Four modules with one job each. `continuity.rs` is a pure ordering state machine. `log_decode.rs` gains payload decoders keyed on `topic0`. `live_state.rs` holds immutable versioned snapshots plus the dirty set. `state_gate.rs` re-anchors sampled pools over RPC in the background and records divergence. Nothing in this phase changes what any scan prices — the store is written and measured, never read for a quote.

**Tech Stack:** Rust 1.97.0, `ethers` 2.0.14 (`Log`, `I256`, `U256`, `H256`), `dashmap` 6, `tokio` 1, `prometheus` 0.13. Tests inline per module via `#[cfg(test)] mod tests`.

## Global Constraints

- Toolchain pinned at `1.97.0`. Do not bump.
- No new runtime dependencies. `loom` may be added as a **dev**-dependency in Task 5 only.
- Every module declared twice: `pub mod x;` in `src/lib.rs` and `mod x;` in `src/main.rs`. This is an established pattern, not a mistake.
- Items used only by lib/tests need `#[allow(dead_code)]` or they warn in the `main.rs` build.
- Never hold a `std::sync::Mutex` guard across an `.await`.
- **Dispatch on `topic0`, never on a venue label** (spec §3.1). `data/base/aerodrome_slipstream/pools.jsonl` is ~half Solidly V2 pairs mislabeled as CL; a log is self-describing, an inventory is not.
- **Do not write a PancakeSwap V3 decoder.** Its `0x19b47279…` topic and payload are unverified (spec §10). Pancake pools are simply not decoded this phase.
- `cargo fmt` must NOT be run — the repo has never been formatted and it would rewrite every file. `cargo clippy --all-targets` must introduce no NEW warnings; `edge_capacity_from_cl_state` in `venues.rs` is pre-existing.
- Verify with `cargo test --lib --bins --tests` plus `cargo test --doc`.

## Scope

**In:** decoders for V2 `Sync` (both topic variants) and CL `Swap` (UniV3 + Slipstream, one shared topic); the continuity cursor; versioned snapshots and the lossless dirty set; the state gate; shadow-mode reconciliation records and metrics.

**Explicitly out, and why:**
- **`ScanSnapshot` generation protocol (spec §4.2)** and **candidate staleness guards (§6, `VersionChecked`)** — both govern *reading* state for pricing. Nothing prices from the store this phase. They belong to Phase 2.
- **Tick ladders and CL balances.** `Mint`/`Burn` decoding and `balance0`/`balance1` delta tracking are deferred; a CL snapshot here is `sqrt_price_x96` + `liquidity` + `tick`, which is exactly what `slot0()` returns and therefore what the gate can check.
- **Balancer, Curve, PancakeSwap V3.** Unchanged, RPC path, not decoded.

**Reference spec:** `docs/superpowers/specs/2026-08-29-live-state-dirty-scan-design.md`

---

### Task 1: Continuity cursor

A pure ordering state machine. Lives in its own module rather than inside `live_state.rs` because it has one job and no dependencies, and `live_state.rs` is already the largest piece of this phase.

**Files:**
- Create: `src/continuity.rs`
- Modify: `src/lib.rs`, `src/main.rs` (module declarations)

**Interfaces:**
- Consumes: nothing.
- Produces: `Ordinal { block: u64, tx_index: u64, log_index: u64 }` (derives `Ord`), `enum Observation { Accept, Duplicate, Break(BreakReason) }`, `enum BreakReason { OutOfOrder, Reorg }`, `Cursor::new()`, `Cursor::observe(&mut self, Ordinal, removed: bool) -> Observation`, `Cursor::last(&self) -> Option<Ordinal>`.

- [ ] **Step 1: Write the failing test**

Create `src/continuity.rs`:

```rust
//! Ordering state machine for the log stream.
//!
//! Detects duplicates, backwards movement and reorgs. It deliberately does NOT
//! detect MISSING logs: the subscription is filtered, so consecutive
//! `log_index` values are not expected and a gap carries no information. A
//! dropped log is caught downstream by `state_gate` divergence, not here — see
//! spec §8 I1, whose lossless claim covers the dirty set, not delivery.

#[cfg(test)]
mod tests {
    use super::*;

    fn ord(b: u64, t: u64, l: u64) -> Ordinal {
        Ordinal { block: b, tx_index: t, log_index: l }
    }

    #[test]
    fn ordinals_compare_lexicographically() {
        assert!(ord(1, 0, 0) < ord(1, 0, 1));
        assert!(ord(1, 0, 9) < ord(1, 1, 0), "tx_index outranks log_index");
        assert!(ord(1, 9, 9) < ord(2, 0, 0), "block outranks everything");
    }

    #[test]
    fn the_first_observation_is_always_accepted() {
        let mut c = Cursor::new();
        assert_eq!(c.observe(ord(10, 0, 0), false), Observation::Accept);
        assert_eq!(c.last(), Some(ord(10, 0, 0)));
    }

    #[test]
    fn strictly_greater_advances() {
        let mut c = Cursor::new();
        c.observe(ord(10, 0, 0), false);
        assert_eq!(c.observe(ord(10, 0, 1), false), Observation::Accept);
        assert_eq!(c.observe(ord(11, 0, 0), false), Observation::Accept);
    }

    /// A replayed log must change nothing at all — no version bump, no dirty
    /// mark. Idempotence is what makes reconnect-and-replay safe.
    #[test]
    fn an_equal_ordinal_is_a_duplicate() {
        let mut c = Cursor::new();
        c.observe(ord(10, 2, 3), false);
        assert_eq!(c.observe(ord(10, 2, 3), false), Observation::Duplicate);
        assert_eq!(c.last(), Some(ord(10, 2, 3)), "cursor must not move");
    }

    #[test]
    fn a_lower_ordinal_breaks_continuity() {
        let mut c = Cursor::new();
        c.observe(ord(10, 2, 3), false);
        assert_eq!(
            c.observe(ord(10, 2, 2), false),
            Observation::Break(BreakReason::OutOfOrder)
        );
    }

    /// A dropped preconfirmation and a reorg are the same signal.
    #[test]
    fn a_removed_log_breaks_continuity_even_when_ordered() {
        let mut c = Cursor::new();
        c.observe(ord(10, 0, 0), false);
        assert_eq!(
            c.observe(ord(11, 0, 0), true),
            Observation::Break(BreakReason::Reorg)
        );
    }

    /// After a break the cursor must re-baseline, or every subsequent log
    /// compares against a position the chain has abandoned.
    #[test]
    fn a_break_resets_the_baseline() {
        let mut c = Cursor::new();
        c.observe(ord(10, 5, 5), false);
        c.observe(ord(9, 0, 0), false);
        assert_eq!(c.last(), None, "break clears the cursor");
        assert_eq!(c.observe(ord(9, 0, 1), false), Observation::Accept);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib continuity`
Expected: FAIL to compile — `cannot find type Ordinal in this scope`, and `continuity` is not a known module.

- [ ] **Step 3: Write minimal implementation**

Insert above the test module in `src/continuity.rs`:

```rust
/// Position of a log in the chain's total order.
///
/// Field order IS the comparison order — `derive(Ord)` compares block, then
/// tx_index, then log_index, which is exactly the lexicographic rule spec §4.4
/// specifies. When flashblocks land this gains `payload_id` and
/// `flashblock_index` ahead of `block`; the state machine below does not change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ordinal {
    pub block: u64,
    pub tx_index: u64,
    pub log_index: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreakReason {
    OutOfOrder,
    Reorg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    Accept,
    Duplicate,
    Break(BreakReason),
}

#[derive(Debug, Default)]
pub struct Cursor {
    last: Option<Ordinal>,
}

// main.rs compiles its own copy of this module.
#[allow(dead_code)]
impl Cursor {
    pub fn new() -> Self {
        Self { last: None }
    }

    pub fn last(&self) -> Option<Ordinal> {
        self.last
    }

    /// Classify an incoming log and advance the cursor.
    ///
    /// A break clears the baseline: after a reorg or a backwards jump the old
    /// position describes a chain state that no longer exists, so comparing
    /// against it would reject every subsequent log.
    pub fn observe(&mut self, ordinal: Ordinal, removed: bool) -> Observation {
        if removed {
            self.last = None;
            return Observation::Break(BreakReason::Reorg);
        }
        match self.last {
            None => {
                self.last = Some(ordinal);
                Observation::Accept
            }
            Some(prev) if ordinal > prev => {
                self.last = Some(ordinal);
                Observation::Accept
            }
            Some(prev) if ordinal == prev => Observation::Duplicate,
            Some(_) => {
                self.last = None;
                Observation::Break(BreakReason::OutOfOrder)
            }
        }
    }
}
```

Add `pub mod continuity;` to `src/lib.rs` (after `pub mod convex;`) and `mod continuity;` to `src/main.rs` (after `mod convex;`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib continuity`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add src/continuity.rs src/lib.rs src/main.rs
git commit -m "feat(continuity): add the log ordering state machine

Detects duplicates, backwards movement and reorgs. Deliberately does not
claim to detect missing logs: the subscription is filtered, so an index gap
carries no information.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: V2 `Sync` payload decoder

One decoder serves both topics. `uint112` and `uint256` are both ABI-encoded as 32-byte words, so the payload layout is identical — only the topic differs.

**Files:**
- Modify: `src/log_decode.rs`

**Interfaces:**
- Consumes: `TOPIC_V2_SYNC`, `TOPIC_SOLIDLY_SYNC` (already present).
- Produces: `pub struct V2SyncDelta { pub reserve0: U256, pub reserve1: U256 }`, `pub fn decode_v2_sync(log: &Log) -> Option<V2SyncDelta>`, `pub fn word(data: &[u8], index: usize) -> Option<[u8; 32]>`.

- [ ] **Step 1: Write the failing test**

Add inside the existing `mod tests` in `src/log_decode.rs`:

```rust
    use ethers::types::{Address, Bytes, Log};

    fn log_with(topic: H256, data: Vec<u8>) -> Log {
        Log {
            address: Address::from_low_u64_be(1),
            topics: vec![topic],
            data: Bytes::from(data),
            ..Default::default()
        }
    }

    /// Two 32-byte words, big-endian.
    fn words(vals: &[u128]) -> Vec<u8> {
        let mut out = Vec::new();
        for v in vals {
            let mut w = [0u8; 32];
            w[16..].copy_from_slice(&v.to_be_bytes());
            out.extend_from_slice(&w);
        }
        out
    }

    #[test]
    fn decodes_univ2_sync_reserves() {
        let log = log_with(*TOPIC_V2_SYNC, words(&[111, 222]));
        let d = decode_v2_sync(&log).expect("should decode");
        assert_eq!(d.reserve0, U256::from(111u64));
        assert_eq!(d.reserve1, U256::from(222u64));
    }

    /// Same payload shape, different topic — one decoder covers both families.
    #[test]
    fn decodes_solidly_sync_with_the_same_layout() {
        let log = log_with(*TOPIC_SOLIDLY_SYNC, words(&[333, 444]));
        let d = decode_v2_sync(&log).expect("should decode");
        assert_eq!(d.reserve0, U256::from(333u64));
        assert_eq!(d.reserve1, U256::from(444u64));
    }

    #[test]
    fn refuses_a_foreign_topic() {
        let log = log_with(*TOPIC_V2_SWAP, words(&[1, 2]));
        assert!(decode_v2_sync(&log).is_none(), "topic0 decides, nothing else");
    }

    /// A truncated payload must decline rather than read garbage or panic.
    #[test]
    fn refuses_a_short_payload() {
        let log = log_with(*TOPIC_V2_SYNC, vec![0u8; 63]);
        assert!(decode_v2_sync(&log).is_none());
    }

    #[test]
    fn refuses_a_log_with_no_topics() {
        let mut log = log_with(*TOPIC_V2_SYNC, words(&[1, 2]));
        log.topics.clear();
        assert!(decode_v2_sync(&log).is_none());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib log_decode`
Expected: FAIL to compile — `cannot find function decode_v2_sync in this scope`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/log_decode.rs`, above the test module. Extend the `use` line to `use ethers::types::{Log, H256, U256};`:

```rust
/// The `index`-th 32-byte ABI word of `data`, or `None` if it is not there.
pub fn word(data: &[u8], index: usize) -> Option<[u8; 32]> {
    let start = index.checked_mul(32)?;
    let end = start.checked_add(32)?;
    let slice = data.get(start..end)?;
    let mut out = [0u8; 32];
    out.copy_from_slice(slice);
    Some(out)
}

/// Reserves after a `Sync`, for either pool family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V2SyncDelta {
    pub reserve0: U256,
    pub reserve1: U256,
}

/// Decode a `Sync` from either UniV2 (`uint112`) or Solidly (`uint256`).
///
/// The payload layout is identical — ABI pads both widths to 32 bytes — so the
/// topic is the only thing that differs, and this accepts both. `Sync` carries
/// COMPLETE state rather than a delta, which is why V2 needs no drift budget.
pub fn decode_v2_sync(log: &Log) -> Option<V2SyncDelta> {
    let topic = log.topics.first()?;
    if topic != &*TOPIC_V2_SYNC && topic != &*TOPIC_SOLIDLY_SYNC {
        return None;
    }
    Some(V2SyncDelta {
        reserve0: U256::from_big_endian(&word(&log.data, 0)?),
        reserve1: U256::from_big_endian(&word(&log.data, 1)?),
    })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib log_decode`
Expected: PASS — 5 pre-existing tests plus 5 new.

- [ ] **Step 5: Commit**

```bash
git add src/log_decode.rs
git commit -m "feat(log-decode): decode V2 Sync reserves for both pool families

One decoder, two topics: uint112 and uint256 are both ABI-padded to 32
bytes, so only topic0 differs. Sync carries complete state, not a delta.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: CL `Swap` payload decoder

**Files:**
- Modify: `src/log_decode.rs`

**Interfaces:**
- Consumes: `word` from Task 2.
- Produces: `pub static TOPIC_CL_SWAP: LazyLock<H256>`, `pub struct ClSwapDelta { pub amount0: I256, pub amount1: I256, pub sqrt_price_x96: U256, pub liquidity: u128, pub tick: i32 }`, `pub fn decode_cl_swap(log: &Log) -> Option<ClSwapDelta>`.

- [ ] **Step 1: Write the failing test**

Add inside `mod tests` in `src/log_decode.rs`:

```rust
    #[test]
    fn cl_swap_topic_matches_the_value_observed_on_chain() {
        assert_eq!(
            *TOPIC_CL_SWAP,
            topic_of("Swap(address,address,int256,int256,uint160,uint128,int24)")
        );
        assert_eq!(
            format!("{:#x}", *TOPIC_CL_SWAP),
            "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
        );
    }

    /// Real Base log, block 0x3049142, pool 0x4e392fbfe4d0557c82d2f97f02ec39daa31516dd.
    /// amount1 and tick are negative, which is the case a naive unsigned read
    /// gets catastrophically wrong rather than slightly wrong.
    #[test]
    fn decodes_a_real_cl_swap_including_negative_values() {
        let data = hex::decode(concat!(
            "0000000000000000000000000000000000000000000000000283a6dc44aa9e00",
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffe56d1ffc",
            "0000000000000000000000000000000000000000000340475901e2898ee7248d",
            "00000000000000000000000000000000000000000000000001e42289497d0ed7",
            "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffcf9a2",
        ))
        .expect("fixture hex");
        let log = log_with(*TOPIC_CL_SWAP, data);
        let d = decode_cl_swap(&log).expect("should decode");

        // Values computed from the fixture words, not eyeballed.
        assert_eq!(d.amount0, I256::from(181_171_875_000_000_000i64));
        assert!(d.amount1.is_negative(), "amount1 must decode as negative");
        assert_eq!(d.amount1, I256::from(-445_833_220i64));
        assert_eq!(d.liquidity, 136_271_861_766_754_007u128);
        assert_eq!(d.tick, -198_238, "int24 must sign-extend");
        assert_eq!(
            d.sqrt_price_x96,
            U256::from_dec_str("3930325046233202984166541").expect("sqrt price")
        );
    }

    #[test]
    fn cl_swap_refuses_a_foreign_topic() {
        let log = log_with(*TOPIC_V2_SYNC, vec![0u8; 160]);
        assert!(decode_cl_swap(&log).is_none());
    }

    #[test]
    fn cl_swap_refuses_a_short_payload() {
        let log = log_with(*TOPIC_CL_SWAP, vec![0u8; 159]);
        assert!(
            decode_cl_swap(&log).is_none(),
            "five words are required; a partial read is worse than no read"
        );
    }
```

Add `use ethers::types::I256;` to the test module's imports.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib log_decode::tests::decodes_a_real_cl_swap`
Expected: FAIL to compile — `cannot find value TOPIC_CL_SWAP in this scope`.

- [ ] **Step 3: Write minimal implementation**

Extend the top-level import to `use ethers::types::{I256, Log, H256, U256};` and add:

```rust
/// `Swap(address indexed sender, address indexed recipient, int256 amount0,
///       int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick)`
///
/// Uniswap V3 AND Aerodrome Slipstream. Confirmed on Base at block 0x3049142:
/// six Slipstream pools and one UniV3 pool emitted this same topic in one
/// block, with an identical five-word payload — so ONE decoder covers both.
///
/// PancakeSwap V3 uses a DIFFERENT topic (`0x19b47279…`, two extra trailing
/// fields) which is not yet verified against a real log, so it is not decoded.
pub static TOPIC_CL_SWAP: LazyLock<H256> = LazyLock::new(|| {
    topic_of("Swap(address,address,int256,int256,uint160,uint128,int24)")
});

/// Post-swap CL pool state, straight out of the log.
///
/// `sqrt_price_x96`, `liquidity` and `tick` are the pool's new `slot0`/
/// `liquidity()` — no RPC needed. `amount0`/`amount1` are signed pool-balance
/// deltas, retained for the balance tracking Phase 2 adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClSwapDelta {
    pub amount0: I256,
    pub amount1: I256,
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
}

/// Sign-extend a two's-complement ABI word to `i32`.
///
/// `int24` arrives sign-extended across all 32 bytes, so reading the low bytes
/// unsigned turns tick -198750 into a huge positive number and puts the pool on
/// the wrong side of the curve.
fn word_to_i32(w: [u8; 32]) -> i32 {
    let negative = w[0] & 0x80 != 0;
    let mut v: i64 = 0;
    for b in &w[28..32] {
        v = (v << 8) | i64::from(*b);
    }
    if negative {
        v -= 1i64 << 32;
    }
    v as i32
}

pub fn decode_cl_swap(log: &Log) -> Option<ClSwapDelta> {
    if log.topics.first()? != &*TOPIC_CL_SWAP {
        return None;
    }
    let amount0 = I256::from_raw(U256::from_big_endian(&word(&log.data, 0)?));
    let amount1 = I256::from_raw(U256::from_big_endian(&word(&log.data, 1)?));
    let sqrt_price_x96 = U256::from_big_endian(&word(&log.data, 2)?);
    let liquidity = U256::from_big_endian(&word(&log.data, 3)?).as_u128();
    let tick = word_to_i32(word(&log.data, 4)?);
    Some(ClSwapDelta { amount0, amount1, sqrt_price_x96, liquidity, tick })
}
```

Add `*TOPIC_CL_SWAP` to `monitored_topics()`'s returned vec so the subscription actually asks for it.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib log_decode`
Expected: PASS. If `decodes_a_real_cl_swap_including_negative_values` fails on `tick`, `word_to_i32` is wrong — the fixture's last word is `…fffcf9a2`, which is -198750, not a large positive.

- [ ] **Step 5: Commit**

```bash
git add src/log_decode.rs
git commit -m "feat(log-decode): decode CL Swap for UniV3 and Slipstream

One decoder for both: confirmed on Base that Slipstream emits the identical
0xc42079f9 topic and five-word payload as UniV3. Pancake V3 uses a different,
unverified topic and is deliberately not decoded.

Tick is int24 sign-extended across the full word; reading it unsigned puts
the pool on the wrong side of the curve.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: Trust states and provenance

**Files:**
- Create: `src/live_state.rs` (types only; the store arrives in Task 5)
- Modify: `src/lib.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `crate::continuity::Ordinal`.
- Produces: `TrustState`, `StaleReason`, `UnknownReason`, `Provenance`, `V2Snapshot`, `ClSnapshot`, `may_price_locally(&TrustState) -> bool`.

- [ ] **Step 1: Write the failing test**

Create `src/live_state.rs`:

```rust
//! Live pool state, maintained from decoded logs.
//!
//! In Phase 1 this store is WRITTEN and MEASURED but never read for pricing —
//! see the plan's scope section. `ScanSnapshot` (spec §4.2) and the candidate
//! staleness guards (§6) arrive with Phase 2, when something finally reads it.

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy must be an exhaustive match with no wildcard, so adding a
    /// trust state without deciding its policy fails to COMPILE rather than
    /// silently defaulting to tradable.
    #[test]
    fn only_anchored_and_derived_may_price_locally() {
        assert!(may_price_locally(&TrustState::Anchored));
        assert!(may_price_locally(&TrustState::Derived));
        assert!(!may_price_locally(&TrustState::Stale(StaleReason::AnchorTtlExpired)));
        assert!(!may_price_locally(&TrustState::Stale(StaleReason::DriftBudgetExhausted)));
        assert!(!may_price_locally(&TrustState::Diverged { err_bps: 1 }));
        assert!(!may_price_locally(&TrustState::Unknown(UnknownReason::NeverAnchored)));
        assert!(!may_price_locally(&TrustState::Unknown(UnknownReason::ContinuityBreak)));
        assert!(!may_price_locally(&TrustState::Unknown(UnknownReason::Reorg)));
        assert!(!may_price_locally(&TrustState::Unknown(UnknownReason::WsUnavailable)));
    }

    /// Stale is NOT a flavour of Derived. Conflating them is how a pool that
    /// aged out keeps getting priced locally.
    #[test]
    fn stale_is_not_derived() {
        assert_ne!(
            may_price_locally(&TrustState::Derived),
            may_price_locally(&TrustState::Stale(StaleReason::AnchorTtlExpired))
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib live_state`
Expected: FAIL to compile — `cannot find type TrustState`, module unknown.

- [ ] **Step 3: Write minimal implementation**

Add above the tests in `src/live_state.rs`:

```rust
use crate::continuity::Ordinal;
use crate::quote_univ2::UniV2PairState;
use ethers::types::U256;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnknownReason {
    NeverAnchored,
    ContinuityBreak,
    Reorg,
    WsUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleReason {
    AnchorTtlExpired,
    DriftBudgetExhausted,
}

/// Why a pool may or may not be priced from local state.
///
/// `Diverged` carries its magnitude: a pool measured wrong is a different
/// condition from one that merely aged out, and the size of the disagreement is
/// what tells a decoder bug from an RPC timing artefact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustState {
    Anchored,
    Derived,
    Stale(StaleReason),
    Diverged { err_bps: i64 },
    Unknown(UnknownReason),
}

/// May this state be used to price a route?
///
/// Exhaustive on purpose — NO wildcard arm. A new `TrustState` variant must
/// fail to compile here until someone decides its policy.
pub fn may_price_locally(trust: &TrustState) -> bool {
    match trust {
        TrustState::Anchored | TrustState::Derived => true,
        TrustState::Stale(_) => false,
        TrustState::Diverged { .. } => false,
        TrustState::Unknown(_) => false,
    }
}

/// Where a snapshot came from and what lineage it belongs to.
#[derive(Clone, Copy, Debug)]
pub struct Provenance {
    /// Monotonic per pool, bumped on every accepted update.
    pub state_version: u64,
    /// Identity of the RPC anchor this lineage descends from.
    pub anchor_id: u64,
    /// Global epoch at application time; a break invalidates every snapshot
    /// carrying an older value, in O(1).
    pub continuity_epoch: u64,
    /// Cursor position of the log that produced this, `None` for an anchor.
    pub ordinal: Option<Ordinal>,
    pub anchored_at: Instant,
    pub trust: TrustState,
}

#[derive(Clone, Debug)]
pub struct V2Snapshot {
    pub state: UniV2PairState,
    pub prov: Provenance,
}

/// CL state as carried by a `Swap` log — exactly what `slot0()` plus
/// `liquidity()` return, which is what makes it checkable against RPC.
/// Tick ladders and balances arrive in Phase 2.
#[derive(Clone, Debug)]
pub struct ClSnapshot {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub prov: Provenance,
}
```

Add `pub mod live_state;` to `src/lib.rs` and `mod live_state;` to `src/main.rs`, both after the `continuity` declaration.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib live_state`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add src/live_state.rs src/lib.rs src/main.rs
git commit -m "feat(live-state): add trust states and snapshot provenance

may_price_locally is an exhaustive match with no wildcard, so a new trust
state cannot silently become locally tradable.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: The store — version protocol and lossless dirty set

The heart of the phase. Spec §4.1: apply state, bump version, publish dirty — in that order — and drain by swap.

**Files:**
- Modify: `src/live_state.rs`
- Modify: `Cargo.toml` (add `loom` under `[dev-dependencies]`)

**Interfaces:**
- Consumes: Task 1 `Cursor`/`Observation`, Task 2 `decode_v2_sync`, Task 3 `decode_cl_swap`, Task 4 types.
- Produces: `LiveState::new()`, `apply_log(&self, &Log) -> ApplyOutcome`, `drain_dirty(&self) -> HashMap<Address, u64>`, `anchor_v2`, `anchor_cl`, `v2_snapshot`, `cl_snapshot`, `break_continuity(&self, UnknownReason) -> u64`, `enum ApplyOutcome { Applied { pool: Address, version: u64 }, Duplicate, ContinuityBroken(BreakReason), Undecodable }`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/live_state.rs`:

```rust
    use crate::log_decode::{TOPIC_CL_SWAP, TOPIC_SOLIDLY_SYNC};
    use ethers::types::{Address, Bytes, Log, H256};

    fn sync_log(pool: Address, r0: u128, r1: u128, block: u64, li: u64) -> Log {
        let mut data = Vec::new();
        for v in [r0, r1] {
            let mut w = [0u8; 32];
            w[16..].copy_from_slice(&v.to_be_bytes());
            data.extend_from_slice(&w);
        }
        Log {
            address: pool,
            topics: vec![*TOPIC_SOLIDLY_SYNC],
            data: Bytes::from(data),
            block_number: Some(block.into()),
            transaction_index: Some(0u64.into()),
            log_index: Some(li.into()),
            removed: Some(false),
            ..Default::default()
        }
    }

    #[test]
    fn applying_a_sync_bumps_the_version_and_marks_dirty() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(1);
        match ls.apply_log(&sync_log(pool, 10, 20, 100, 0)) {
            ApplyOutcome::Applied { pool: p, version } => {
                assert_eq!(p, pool);
                assert_eq!(version, 1);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        let snap = ls.v2_snapshot(pool).expect("snapshot");
        assert_eq!(snap.state.reserve0, U256::from(10u64));
        assert_eq!(ls.drain_dirty().get(&pool), Some(&1));
    }

    /// Drain must be a swap, leaving nothing behind and losing nothing.
    #[test]
    fn drain_takes_everything_and_leaves_the_set_empty() {
        let ls = LiveState::new();
        for i in 1..=3u64 {
            ls.apply_log(&sync_log(Address::from_low_u64_be(i), 1, 2, 100, i));
        }
        assert_eq!(ls.drain_dirty().len(), 3);
        assert!(ls.drain_dirty().is_empty(), "second drain sees nothing");
    }

    /// State must be applied BEFORE the pool is published dirty, or a consumer
    /// can drain a pool and price it against the previous value.
    #[test]
    fn state_is_visible_before_the_pool_is_published_dirty() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(7);
        ls.apply_log(&sync_log(pool, 42, 43, 100, 0));
        let batch = ls.drain_dirty();
        let published = *batch.get(&pool).expect("published");
        let snap = ls.v2_snapshot(pool).expect("snapshot");
        assert!(
            snap.prov.state_version >= published,
            "a drained pool's snapshot must never be behind its published version"
        );
    }

    #[test]
    fn a_duplicate_log_changes_nothing() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(2);
        let log = sync_log(pool, 5, 6, 100, 3);
        ls.apply_log(&log);
        ls.drain_dirty();
        assert_eq!(ls.apply_log(&log), ApplyOutcome::Duplicate);
        assert_eq!(ls.v2_snapshot(pool).unwrap().prov.state_version, 1);
        assert!(ls.drain_dirty().is_empty(), "a duplicate must not re-dirty");
    }

    /// One atomic increment invalidates every extant snapshot — no map sweep,
    /// which is what lets the searcher keep running during recovery.
    #[test]
    fn a_continuity_break_invalidates_every_snapshot_in_one_step() {
        let ls = LiveState::new();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        ls.apply_log(&sync_log(a, 1, 2, 100, 0));
        ls.apply_log(&sync_log(b, 3, 4, 100, 1));
        assert!(may_price_locally(&ls.v2_snapshot(a).unwrap().prov.trust));

        ls.break_continuity(UnknownReason::ContinuityBreak);

        for p in [a, b] {
            let t = ls.v2_snapshot(p).unwrap().prov.trust;
            assert_eq!(t, TrustState::Unknown(UnknownReason::ContinuityBreak));
            assert!(!may_price_locally(&t));
        }
    }

    #[test]
    fn a_removed_log_breaks_continuity() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(3);
        ls.apply_log(&sync_log(pool, 1, 2, 100, 0));
        let mut gone = sync_log(pool, 9, 9, 101, 0);
        gone.removed = Some(true);
        assert!(matches!(
            ls.apply_log(&gone),
            ApplyOutcome::ContinuityBroken(_)
        ));
    }

    #[test]
    fn an_unknown_topic_is_undecodable_and_harmless() {
        let ls = LiveState::new();
        let mut log = sync_log(Address::from_low_u64_be(4), 1, 2, 100, 0);
        log.topics = vec![H256::zero()];
        assert_eq!(ls.apply_log(&log), ApplyOutcome::Undecodable);
        assert!(ls.drain_dirty().is_empty());
    }

    #[test]
    fn a_cl_swap_updates_the_cl_snapshot() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(9);
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 32]);           // amount0
        data.extend_from_slice(&[0u8; 32]);           // amount1
        let mut w = [0u8; 32];
        w[31] = 7;
        data.extend_from_slice(&w);                    // sqrtPriceX96 = 7
        let mut l = [0u8; 32];
        l[31] = 5;
        data.extend_from_slice(&l);                    // liquidity = 5
        data.extend_from_slice(&[0u8; 32]);           // tick = 0
        let log = Log {
            address: pool,
            topics: vec![*TOPIC_CL_SWAP],
            data: Bytes::from(data),
            block_number: Some(100u64.into()),
            transaction_index: Some(0u64.into()),
            log_index: Some(0u64.into()),
            removed: Some(false),
            ..Default::default()
        };
        assert!(matches!(ls.apply_log(&log), ApplyOutcome::Applied { .. }));
        let snap = ls.cl_snapshot(pool).expect("cl snapshot");
        assert_eq!(snap.liquidity, 5);
        assert_eq!(snap.sqrt_price_x96, U256::from(7u64));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib live_state`
Expected: FAIL to compile — `cannot find function LiveState::new`.

- [ ] **Step 3: Write minimal implementation**

Extend the imports at the top of `src/live_state.rs`:

```rust
use crate::continuity::{BreakReason, Cursor, Observation, Ordinal};
use crate::log_decode::{decode_cl_swap, decode_v2_sync};
use crate::quote_univ2::UniV2PairState;
use dashmap::DashMap;
use ethers::types::{Address, Log, U256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
```

Then add:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied { pool: Address, version: u64 },
    Duplicate,
    ContinuityBroken(BreakReason),
    Undecodable,
}

#[derive(Default)]
pub struct LiveState {
    v2: DashMap<Address, Arc<V2Snapshot>>,
    cl: DashMap<Address, Arc<ClSnapshot>>,
    /// pool -> highest published version. A `HashMap` behind a mutex, because
    /// the drain must be one atomic swap: collect-then-clear erases any mark
    /// landing between the two, which is permanent loss, not delay.
    dirty: StdMutex<HashMap<Address, u64>>,
    cursor: StdMutex<Cursor>,
    next_version: AtomicU64,
    continuity_epoch: AtomicU64,
    next_anchor_id: AtomicU64,
    /// Bumped on every accepted application and on every epoch change. Phase 2
    /// uses this for `ScanSnapshot` generation validation (spec §4.2).
    generation: AtomicU64,
}

#[allow(dead_code)]
impl LiveState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn continuity_epoch(&self) -> u64 {
        self.continuity_epoch.load(Ordering::SeqCst)
    }

    fn ordinal_of(log: &Log) -> Option<Ordinal> {
        Some(Ordinal {
            block: log.block_number?.as_u64(),
            tx_index: log.transaction_index?.as_u64(),
            log_index: log.log_index?.as_u64(),
        })
    }

    fn provenance(&self, version: u64, ordinal: Option<Ordinal>, trust: TrustState) -> Provenance {
        Provenance {
            state_version: version,
            anchor_id: self.next_anchor_id.load(Ordering::SeqCst),
            continuity_epoch: self.continuity_epoch(),
            ordinal,
            anchored_at: Instant::now(),
            trust,
        }
    }

    /// Publish a pool as dirty at `version`, keeping the highest.
    ///
    /// Called only AFTER the snapshot is in the map, so a consumer that drains
    /// this pool always sees state at least as new as the version published.
    fn publish(&self, pool: Address, version: u64) {
        if let Ok(mut guard) = self.dirty.lock() {
            let slot = guard.entry(pool).or_insert(version);
            if *slot < version {
                *slot = version;
            }
        }
    }

    /// Take the dirty set and leave an empty one, atomically.
    pub fn drain_dirty(&self) -> HashMap<Address, u64> {
        match self.dirty.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(_) => HashMap::new(),
        }
    }

    /// Invalidate every snapshot with ONE atomic increment.
    ///
    /// No map sweep and no per-pool writes: snapshots carry the epoch they were
    /// applied under, so bumping it makes all of them `Unknown` at once. That is
    /// what lets recovery be background work while the searcher keeps running.
    pub fn break_continuity(&self, reason: UnknownReason) -> u64 {
        let epoch = self.continuity_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut c) = self.cursor.lock() {
            *c = Cursor::new();
        }
        tracing::warn!(?reason, epoch, "continuity broken; all local state untrusted");
        epoch
    }

    /// Trust for a snapshot, resolved against the CURRENT epoch.
    fn resolve_trust(&self, prov: &Provenance) -> TrustState {
        if prov.continuity_epoch != self.continuity_epoch() {
            return TrustState::Unknown(UnknownReason::ContinuityBreak);
        }
        prov.trust
    }

    pub fn v2_snapshot(&self, pool: Address) -> Option<V2Snapshot> {
        let entry = self.v2.get(&pool)?;
        let mut snap = (**entry).clone();
        snap.prov.trust = self.resolve_trust(&snap.prov);
        Some(snap)
    }

    pub fn cl_snapshot(&self, pool: Address) -> Option<ClSnapshot> {
        let entry = self.cl.get(&pool)?;
        let mut snap = (**entry).clone();
        snap.prov.trust = self.resolve_trust(&snap.prov);
        Some(snap)
    }

    pub fn anchor_v2(&self, pool: Address, state: UniV2PairState) {
        let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
        self.next_anchor_id.fetch_add(1, Ordering::SeqCst);
        let prov = self.provenance(version, None, TrustState::Anchored);
        self.v2.insert(pool, Arc::new(V2Snapshot { state, prov }));
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn anchor_cl(&self, pool: Address, sqrt_price_x96: U256, liquidity: u128, tick: i32) {
        let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
        self.next_anchor_id.fetch_add(1, Ordering::SeqCst);
        let prov = self.provenance(version, None, TrustState::Anchored);
        self.cl.insert(
            pool,
            Arc::new(ClSnapshot { sqrt_price_x96, liquidity, tick, prov }),
        );
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Decode a log and apply it.
    ///
    /// Order is load-bearing (spec §4.1):
    ///   accept -> build snapshot -> bump version -> swap into map -> publish dirty
    pub fn apply_log(&self, log: &Log) -> ApplyOutcome {
        let Some(ordinal) = Self::ordinal_of(log) else {
            return ApplyOutcome::Undecodable;
        };
        let removed = log.removed.unwrap_or(false);

        let observation = match self.cursor.lock() {
            Ok(mut c) => c.observe(ordinal, removed),
            Err(_) => return ApplyOutcome::Undecodable,
        };
        match observation {
            Observation::Duplicate => return ApplyOutcome::Duplicate,
            Observation::Break(reason) => {
                let r = match reason {
                    BreakReason::Reorg => UnknownReason::Reorg,
                    BreakReason::OutOfOrder => UnknownReason::ContinuityBreak,
                };
                self.break_continuity(r);
                return ApplyOutcome::ContinuityBroken(reason);
            }
            Observation::Accept => {}
        }

        let pool = log.address;
        if let Some(d) = decode_v2_sync(log) {
            let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
            let existing = self.v2.get(&pool).map(|e| e.state.clone());
            let state = UniV2PairState {
                token0: existing.as_ref().map(|s| s.token0).unwrap_or_default(),
                token1: existing.as_ref().map(|s| s.token1).unwrap_or_default(),
                reserve0: d.reserve0,
                reserve1: d.reserve1,
            };
            let prov = self.provenance(version, Some(ordinal), TrustState::Derived);
            self.v2.insert(pool, Arc::new(V2Snapshot { state, prov }));
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.publish(pool, version);
            return ApplyOutcome::Applied { pool, version };
        }

        if let Some(d) = decode_cl_swap(log) {
            let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
            let prov = self.provenance(version, Some(ordinal), TrustState::Derived);
            self.cl.insert(
                pool,
                Arc::new(ClSnapshot {
                    sqrt_price_x96: d.sqrt_price_x96,
                    liquidity: d.liquidity,
                    tick: d.tick,
                    prov,
                }),
            );
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.publish(pool, version);
            return ApplyOutcome::Applied { pool, version };
        }

        ApplyOutcome::Undecodable
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib live_state`
Expected: PASS, 10 tests.

- [ ] **Step 5: Add the loom model for I1**

Add to `Cargo.toml` under `[dev-dependencies]`:

```toml
loom = "0.7"
```

Create `src/bin/../` — no: add to `src/live_state.rs` at the end, outside the existing test module:

```rust
/// Exhaustive interleaving model for spec §8 I1.
///
/// The assertion is about ABSENCE OF LOST VERSIONS, not presence of dirty
/// entries: coalescing and redundant processing are permitted, erasure of the
/// newest transition is not. Multiple writers are modelled even though
/// production has one ingestion writer, so the store stays correct if venue
/// streams are parallelised later.
///
/// Run with: `RUSTFLAGS="--cfg loom" cargo test --lib loom_dirty`
#[cfg(all(test, loom))]
mod loom_dirty {
    use loom::sync::atomic::{AtomicU64, Ordering};
    use loom::sync::{Arc, Mutex};
    use std::collections::HashMap;

    #[test]
    fn no_published_version_is_ever_erased() {
        loom::model(|| {
            let dirty: Arc<Mutex<HashMap<u64, u64>>> = Arc::new(Mutex::new(HashMap::new()));
            let version = Arc::new(AtomicU64::new(0));
            let drained: Arc<Mutex<HashMap<u64, u64>>> = Arc::new(Mutex::new(HashMap::new()));

            let writers: Vec<_> = (0..2)
                .map(|_| {
                    let dirty = dirty.clone();
                    let version = version.clone();
                    loom::thread::spawn(move || {
                        let v = version.fetch_add(1, Ordering::SeqCst) + 1;
                        let mut g = dirty.lock().unwrap();
                        let slot = g.entry(0).or_insert(v);
                        if *slot < v {
                            *slot = v;
                        }
                    })
                })
                .collect();

            let reader = {
                let dirty = dirty.clone();
                let drained = drained.clone();
                loom::thread::spawn(move || {
                    let batch = std::mem::take(&mut *dirty.lock().unwrap());
                    let mut d = drained.lock().unwrap();
                    for (k, v) in batch {
                        let e = d.entry(k).or_insert(v);
                        if *e < v {
                            *e = v;
                        }
                    }
                })
            };

            for w in writers {
                w.join().unwrap();
            }
            reader.join().unwrap();

            let tail = std::mem::take(&mut *dirty.lock().unwrap());
            let mut d = drained.lock().unwrap();
            for (k, v) in tail {
                let e = d.entry(k).or_insert(v);
                if *e < v {
                    *e = v;
                }
            }
            let highest = version.load(Ordering::SeqCst);
            assert_eq!(
                d.get(&0).copied(),
                Some(highest),
                "the newest published version must survive every interleaving"
            );
        });
    }
}
```

- [ ] **Step 6: Run the loom model**

Run: `RUSTFLAGS="--cfg loom" cargo test --lib loom_dirty -- --nocapture`
Expected: PASS. Loom explores every interleaving; a failure prints the schedule that lost a version.

Run: `cargo test --lib live_state` (without the cfg) — expected PASS, loom module not compiled.

- [ ] **Step 7: Commit**

```bash
git add src/live_state.rs Cargo.toml
git commit -m "feat(live-state): version protocol, lossless dirty set, O(1) invalidation

Apply-then-publish ordering with a swap-based drain, so a mark landing after
a drain belongs to the next batch and cannot be erased. Continuity breaks
invalidate every snapshot with one atomic increment rather than a map sweep.

Includes a loom model asserting absence of lost versions under multiple
writers, per spec 8 I1.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: Subscribe to CL pools and topics

Without this the CL decoders never see a log: the filter currently carries only the 23 constant-product pool addresses.

**Files:**
- Modify: `src/ingestion.rs` (`MonitoredPool`, `pool_log_filter`)
- Modify: `src/main.rs:12780-12800` area (build the monitored set from CL inventories too)

**Interfaces:**
- Consumes: `monitored_topics()` (now includes `TOPIC_CL_SWAP` from Task 3).
- Produces: `PoolMonitorKind::ConcentratedLiquidity` variant.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/ingestion.rs`:

```rust
    fn cl_monitored(n: u64) -> MonitoredPool {
        MonitoredPool {
            pair: Address::from_low_u64_be(n),
            token_in: Address::from_low_u64_be(n + 1000),
            token_out: Address::from_low_u64_be(n + 2000),
            fee_bps: 500,
            stable: false,
            kind: PoolMonitorKind::ConcentratedLiquidity,
        }
    }

    #[test]
    fn filter_includes_cl_pools_and_the_cl_swap_topic() {
        let filter = pool_log_filter(&[monitored(1), cl_monitored(2)]);
        let topics = topic0_of(&filter);
        assert!(
            topics.contains(&*crate::log_decode::TOPIC_CL_SWAP),
            "CL Swap topic missing: {topics:?}"
        );
        let addrs = addresses_of(&filter);
        assert!(addrs.contains(&Address::from_low_u64_be(2)), "CL pool not subscribed");
        assert_eq!(topics.len(), 5, "4 constant-product topics + 1 CL");
    }

    /// The poller reads getReserves, which reverts on a CL pool. Polling one
    /// wastes a round-trip every cycle and logs a failure that looks like an
    /// RPC problem.
    #[test]
    fn cl_pools_are_excluded_from_the_reserves_poll() {
        let pools = vec![monitored(1), cl_monitored(2), monitored(3)];
        let pairs = pollable_pairs(&pools, &HashSet::new());
        assert_eq!(pairs.len(), 2);
        assert!(!pairs.contains(&Address::from_low_u64_be(2)));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ingestion::tests::filter_includes_cl`
Expected: FAIL to compile — `no variant named ConcentratedLiquidity found for enum PoolMonitorKind`.

- [ ] **Step 3: Write minimal implementation**

In `src/ingestion.rs`, extend the enum:

```rust
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolMonitorKind {
    UniV2,
    Solidly,
    /// UniV3 / Aerodrome Slipstream. Subscribed for logs, but NOT polled: it
    /// has no `getReserves`, and its state arrives via `Swap` instead.
    ConcentratedLiquidity,
}
```

Change `pollable_pairs` to skip CL pools:

```rust
fn pollable_pairs(pools: &[MonitoredPool], ignored: &HashSet<Address>) -> Vec<Address> {
    pools
        .iter()
        .filter(|p| p.kind != PoolMonitorKind::ConcentratedLiquidity)
        .map(|p| p.pair)
        .filter(|pair| !ignored.contains(pair))
        .collect()
}
```

`pool_log_filter` needs no change — it already uses `monitored_topics()`, which Task 3 extended, and it already takes every pool's address.

In `src/main.rs`, where `monitored` is built (search for `let monitored = venues::merge_monitored_pools(`), append the CL inventories before the `if monitored.is_empty()` check:

```rust
        // CL pools are subscribed for logs but never polled. Without them the
        // Swap decoder never sees a log: the subscription would carry 23 of
        // ~985 Base pools.
        let mut monitored = monitored;
        for records in [&hot_univ3_pools, &hot_slipstream_pools] {
            for r in records.read().await.iter() {
                monitored.push(MonitoredPool {
                    pair: r.pool,
                    token_in: r.token0,
                    token_out: r.token1,
                    fee_bps: r.fee,
                    stable: false,
                    kind: ingestion::PoolMonitorKind::ConcentratedLiquidity,
                });
            }
        }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib ingestion::tests && cargo build --all-targets 2>&1 | grep -E "^error" | head`
Expected: tests PASS, no build errors.

- [ ] **Step 5: Check the address-list size against the provider**

Run the bot for 90 s and confirm the subscription still connects and delivers:

```bash
grep -aE "pool monitor websocket connected|NO logs" <log> | tail -3
```

Expected: `connected pools=<~750+>` and NO silence warning. If the warning fires, the provider is rejecting or truncating a large address list — record the pool count at which it breaks and split the subscription into chunks before continuing.

- [ ] **Step 6: Commit**

```bash
git add src/ingestion.rs src/main.rs
git commit -m "feat(ingestion): subscribe CL pools for Swap logs

The filter carried 23 of ~985 Base pools, so CL state had no event source at
all. CL pools are subscribed but excluded from the getReserves poll, which
would revert on them.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 7: State gate

Mirrors `ParityGate`'s shape deliberately. `ParityGate` asks "is the math right given the state"; this asks "is the state right at all".

**Files:**
- Create: `src/state_gate.rs`
- Modify: `src/lib.rs`, `src/main.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `StateGate::from_env()`, `trusted(Address) -> bool`, `record(Address, Option<i64>)`, `due_for_check(impl IntoIterator<Item = Address>) -> Vec<Address>`, `stats() -> (usize, usize, usize)`, `pub fn divergence_bps(local: U256, on_chain: U256) -> Option<i64>`.

- [ ] **Step 1: Write the failing test**

Create `src/state_gate.rs` with only the tests:

```rust
//! Per-pool trust gate for LOG-DERIVED STATE.
//!
//! Distinct from `cl_parity_gate`, which validates the multi-tick MATH against
//! the pool's own quoter given fresh state. This validates the STATE itself:
//! log-derived snapshot versus a fresh RPC read. Different causes, different
//! fixes, different TTLs — so a single verdict could not tell you which tripped.
//!
//! Fails CLOSED: a pool with no verdict is not trusted.

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::Address;

    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    fn gate(max_err_bps: i64, ttl_secs: u64) -> StateGate {
        StateGate::for_test(max_err_bps, ttl_secs, 8)
    }

    #[test]
    fn an_unmeasured_pool_is_not_trusted() {
        let g = gate(5, 300);
        assert!(!g.trusted(addr(1)), "trusting the unmeasured is the failure this exists to prevent");
    }

    #[test]
    fn a_passing_measurement_grants_trust() {
        let g = gate(5, 300);
        g.record(addr(1), Some(3));
        assert!(g.trusted(addr(1)));
    }

    #[test]
    fn a_failing_measurement_withholds_trust() {
        let g = gate(5, 300);
        g.record(addr(1), Some(4_000));
        assert!(!g.trusted(addr(1)));
    }

    #[test]
    fn under_reporting_fails_too() {
        let g = gate(5, 300);
        g.record(addr(1), Some(-4_000));
        assert!(!g.trusted(addr(1)));
    }

    /// A failed RPC is NOT evidence of correctness. It must write no verdict,
    /// leaving the TTL to age the pool into Stale on its own — failure
    /// downgrades trust, it never stalls the searcher.
    #[test]
    fn an_unreachable_rpc_is_not_evidence() {
        let g = gate(5, 300);
        g.record(addr(1), None);
        assert!(!g.trusted(addr(1)));
        assert!(g.due_for_check([addr(1)]).contains(&addr(1)));
    }

    #[test]
    fn trust_expires_with_the_ttl() {
        let g = gate(5, 0);
        g.record(addr(1), Some(0));
        assert!(!g.trusted(addr(1)), "a verdict is evidence with a shelf life");
    }

    #[test]
    fn due_for_check_is_bounded_and_skips_fresh_pools() {
        let g = gate(5, 300);
        g.record(addr(1), Some(0));
        let due = g.due_for_check((1..=20).map(addr));
        assert_eq!(due.len(), 8, "must respect checks_per_scan");
        assert!(!due.contains(&addr(1)));
    }

    #[test]
    fn divergence_is_signed_and_symmetric() {
        assert_eq!(divergence_bps(U256::from(101u64), U256::from(100u64)), Some(100));
        assert_eq!(divergence_bps(U256::from(99u64), U256::from(100u64)), Some(-100));
        assert_eq!(divergence_bps(U256::from(100u64), U256::from(100u64)), Some(0));
    }

    /// A zero reference is exactly the catastrophic case, so it must decline
    /// rather than be silently treated as agreement.
    #[test]
    fn divergence_declines_on_a_zero_reference() {
        assert_eq!(divergence_bps(U256::from(1u64), U256::zero()), None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib state_gate`
Expected: FAIL to compile — `cannot find type StateGate`.

- [ ] **Step 3: Write minimal implementation**

Add above the tests:

```rust
use ethers::types::{Address, U256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
struct Verdict {
    err_bps: i64,
    trusted: bool,
    checked_at: Instant,
}

pub struct StateGate {
    verdicts: Mutex<HashMap<Address, Verdict>>,
    ttl: Duration,
    max_err_bps: i64,
    checks_per_scan: usize,
}

pub fn gate() -> &'static StateGate {
    static GATE: OnceLock<StateGate> = OnceLock::new();
    GATE.get_or_init(StateGate::from_env)
}

#[allow(dead_code)]
impl StateGate {
    pub fn from_env() -> Self {
        Self {
            ttl: Duration::from_secs(
                crate::util::env_parse_opt::<u64>("ARBOT_STATE_GATE_TTL_SECS")
                    .unwrap_or(300)
                    .max(30),
            ),
            // Matches ARBOT_CL_PARITY_MAX_ERR_BPS so the two gates cannot
            // disagree about what "passing" means.
            max_err_bps: i64::from(
                crate::util::env_parse_opt::<u32>("ARBOT_STATE_GATE_MAX_ERR_BPS").unwrap_or(5),
            ),
            checks_per_scan: crate::util::env_parse_opt::<usize>("ARBOT_STATE_GATE_CHECKS_PER_SCAN")
                .unwrap_or(8)
                .clamp(1, 64),
            verdicts: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub fn for_test(max_err_bps: i64, ttl_secs: u64, checks_per_scan: usize) -> Self {
        Self {
            verdicts: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
            max_err_bps,
            checks_per_scan,
        }
    }

    pub fn trusted(&self, pool: Address) -> bool {
        let Ok(guard) = self.verdicts.lock() else {
            return false;
        };
        match guard.get(&pool) {
            Some(v) => v.trusted && v.checked_at.elapsed() < self.ttl,
            None => false,
        }
    }

    fn needs_check(&self, pool: Address) -> bool {
        let Ok(guard) = self.verdicts.lock() else {
            return false;
        };
        match guard.get(&pool) {
            Some(v) => v.checked_at.elapsed() >= self.ttl,
            None => true,
        }
    }

    /// `None` means the read failed. That is NOT evidence of correctness, so
    /// nothing is stored and the pool stays untrusted until a real measurement.
    pub fn record(&self, pool: Address, err_bps: Option<i64>) {
        let Some(err_bps) = err_bps else {
            return;
        };
        let trusted = err_bps.abs() <= self.max_err_bps;
        if let Ok(mut guard) = self.verdicts.lock() {
            guard.insert(pool, Verdict { trusted, checked_at: Instant::now(), err_bps });
        }
        if !trusted {
            tracing::warn!(
                pool = %format!("{pool:#x}"),
                err_bps,
                max_err_bps = self.max_err_bps,
                "log-derived state disagrees with a fresh RPC read"
            );
        }
    }

    pub fn due_for_check(&self, pools: impl IntoIterator<Item = Address>) -> Vec<Address> {
        let mut out = Vec::new();
        for pool in pools {
            if out.len() >= self.checks_per_scan {
                break;
            }
            if self.needs_check(pool) {
                out.push(pool);
            }
        }
        out
    }

    pub fn stats(&self) -> (usize, usize, usize) {
        let Ok(guard) = self.verdicts.lock() else {
            return (0, 0, 0);
        };
        let total = guard.len();
        let trusted = guard
            .values()
            .filter(|v| v.trusted && v.checked_at.elapsed() < self.ttl)
            .count();
        (trusted, total.saturating_sub(trusted), total)
    }
}

/// Divergence of `local` from `on_chain` in bps, signed.
///
/// `None` when the ratio is not representable — precisely the catastrophic
/// case, so it must never read as agreement.
pub fn divergence_bps(local: U256, on_chain: U256) -> Option<i64> {
    if on_chain.is_zero() {
        return None;
    }
    let (diff, sign) = if local >= on_chain {
        (local - on_chain, 1i64)
    } else {
        (on_chain - local, -1i64)
    };
    let scaled = diff.checked_mul(U256::from(10_000u64))?;
    let bps = scaled / on_chain;
    if bps > U256::from(u64::MAX) {
        return None;
    }
    i64::try_from(bps.as_u128()).ok().map(|v| v * sign)
}
```

Declare the module in `src/lib.rs` and `src/main.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib state_gate`
Expected: PASS, 9 tests.

- [ ] **Step 5: Commit**

```bash
git add src/state_gate.rs src/lib.rs src/main.rs
git commit -m "feat(state-gate): per-pool trust for log-derived state

Separate from cl_parity_gate: that validates the math given fresh state,
this validates the state itself. Fails closed; a failed RPC read writes no
verdict so the TTL ages the pool out rather than granting false trust.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 8: Shadow wiring and the reconciliation record

**Files:**
- Modify: `src/ingestion.rs` (`PoolMonitor` gains `live_state`, `handle_log` applies)
- Modify: `src/metrics.rs` (new metrics)
- Modify: `src/main.rs` (construct `LiveState`, spawn the validation task)

**Interfaces:**
- Consumes: everything above.
- Produces: `PoolMonitor::with_live_state(Arc<LiveState>)`, `spawn_state_validation(...)`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/metrics.rs`:

```rust
    #[test]
    fn shadow_metrics_are_registered_and_start_at_zero() {
        let m = Metrics::new().expect("metrics");
        assert_eq!(m.live_state_applied.get(), 0.0);
        assert_eq!(m.live_state_undecodable.get(), 0.0);
        assert_eq!(m.continuity_breaks.get(), 0.0);
        m.live_state_applied.inc();
        assert_eq!(m.live_state_applied.get(), 1.0);
    }
```

If `src/metrics.rs` has no `mod tests`, create one with `use super::*;`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib metrics`
Expected: FAIL to compile — `no field live_state_applied on type Metrics`.

- [ ] **Step 3: Write minimal implementation**

In `src/metrics.rs`, add three fields to `pub struct Metrics` and register them in `new()` following the exact pattern of `ingestion_ws_events`:

```rust
    pub live_state_applied: Counter,
    pub live_state_undecodable: Counter,
    pub continuity_breaks: Counter,
```

```rust
        let live_state_applied = Counter::with_opts(Opts::new(
            "live_state_applied_total",
            "Logs decoded and applied to live pool state",
        ))?;
        registry
            .register(Box::new(live_state_applied.clone()))
            .context("register live_state_applied_total counter")?;

        let live_state_undecodable = Counter::with_opts(Opts::new(
            "live_state_undecodable_total",
            "Logs delivered but not decodable by any known topic",
        ))?;
        registry
            .register(Box::new(live_state_undecodable.clone()))
            .context("register live_state_undecodable_total counter")?;

        let continuity_breaks = Counter::with_opts(Opts::new(
            "continuity_breaks_total",
            "Continuity breaks (reorg or out-of-order log)",
        ))?;
        registry
            .register(Box::new(continuity_breaks.clone()))
            .context("register continuity_breaks_total counter")?;
```

Add all three to the `Self { ... }` literal at the end of `new()`.

In `src/ingestion.rs`, add a field to `PoolMonitor`:

```rust
    /// Shadow-mode live state. `None` disables it entirely; nothing downstream
    /// reads this store in Phase 1.
    live_state: Option<Arc<crate::live_state::LiveState>>,
```

Initialise to `None` in `new()`, and add:

```rust
    pub fn with_live_state(mut self, live: Arc<crate::live_state::LiveState>) -> Self {
        self.live_state = Some(live);
        self
    }
```

In `handle_log`, after `self.mark_touched(pair);`, add:

```rust
        if let Some(live) = &self.live_state {
            use crate::live_state::ApplyOutcome;
            match live.apply_log(&log) {
                ApplyOutcome::Applied { .. } => {
                    if let Some(m) = &self.metrics {
                        m.live_state_applied.inc();
                    }
                }
                ApplyOutcome::Undecodable => {
                    if let Some(m) = &self.metrics {
                        m.live_state_undecodable.inc();
                    }
                }
                ApplyOutcome::ContinuityBroken(_) => {
                    if let Some(m) = &self.metrics {
                        m.continuity_breaks.inc();
                    }
                }
                ApplyOutcome::Duplicate => {}
            }
        }
```

In `src/main.rs`, construct the store next to the monitor and attach it when `ARBOT_LIVE_STATE_SHADOW` is set:

```rust
                Ok(monitor) => {
                    let monitor = if crate::util::env_flag("ARBOT_LIVE_STATE_SHADOW", false) {
                        let live = Arc::new(crate::live_state::LiveState::new());
                        info!("live-state shadow mode enabled; state is recorded, never priced");
                        monitor.with_live_state(live)
                    } else {
                        monitor
                    };
                    let monitor = Arc::new(monitor);
                    monitor.clone().spawn();
                    Some(monitor)
                }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib metrics && cargo build --all-targets 2>&1 | grep -E "^error" | head`
Expected: test PASSES, no build errors.

- [ ] **Step 5: Commit**

```bash
git add src/metrics.rs src/ingestion.rs src/main.rs
git commit -m "feat(live-state): wire shadow-mode application behind a flag

ARBOT_LIVE_STATE_SHADOW applies decoded logs to the store and counts
outcomes. Nothing reads the store for pricing in this phase.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 9: Full verification and the Phase 1 exit gate

**Files:** none modified.

- [ ] **Step 1: Run the whole suite**

Run: `cargo test --lib --bins --tests 2>&1 | tail -20 && cargo test --doc 2>&1 | tail -5`
Expected: all pass, no regressions.

- [ ] **Step 2: Lint**

Run: `cargo clippy --all-targets 2>&1 | grep -E "^warning:|^error:" | grep -v generated`
Expected: only `edge_capacity_from_cl_state`, which is pre-existing. Do NOT run `cargo fmt`.

- [ ] **Step 3: Run the loom model**

Run: `RUSTFLAGS="--cfg loom" cargo test --lib loom_dirty`
Expected: PASS.

- [ ] **Step 4: Field run**

```bash
ARBOT_LIVE_STATE_SHADOW=1 ./target/release/arb-exec > shadow.log 2>&1 &
sleep 300
curl -s localhost:9100/metrics | grep -E "^(live_state_|continuity_breaks|ingestion_ws_events)"
```

Expected after 5 minutes:
- `live_state_applied_total` > 0 and climbing — decoders are working on real logs.
- `live_state_undecodable_total` should be **0 or small**. A large value means logs are arriving that no decoder handles — most likely Pancake V3 `Swap` (deliberately not decoded) or a venue whose topic we have not identified. Investigate before Phase 2 rather than assuming.
- `continuity_breaks_total` should be **0 or very rare**. Frequent breaks mean the ordinal is wrong or the provider replays; either invalidates the lossless-dirty-set argument.

- [ ] **Step 5: Record the baseline for Phase 2**

The Phase 2 gate (spec §9) is per-venue p99 `relative_delta_bps` ≤ 5 over ≥ 24h. This phase produces the raw material; capture the first hour's `live_state_applied_total` rate and undecodable ratio in the spec's §9.1 record format so Phase 2 has a baseline to compare against.

---

## Self-Review

**Spec coverage.** §3.1 log_decode → Tasks 2, 3 (topic dispatch, no Pancake). §3.2 live_state store → Tasks 4, 5. §3.3 state_gate → Task 7. §4.1 version protocol → Task 5. §4.3 trust states and lineage → Tasks 4, 5. §4.4 continuity cursor → Tasks 1, 5. §4.5 validation off the hot path → Task 7 (gate is a pure read; the RPC anchor task is Phase 2's to schedule). §8 I1 → Task 5 loom model. §8 I4 → Task 4 exhaustive match + Task 5 epoch invalidation. §9 Phase 1 gate → Task 9. §9.2 metrics → Task 8.

**Deliberately deferred, with reasons stated in Scope:** §4.2 `ScanSnapshot`, §6 candidate guards and `VersionChecked`, §8 I2/I3/I5, tick ladders, CL balances, Pancake. All are Phase 2 or later because nothing reads the store for pricing here.

**Known gap, stated rather than hidden.** §8 I1 claims no state update can be silently lost. This phase proves that for the dirty set (loom, Task 5) but NOT for delivery: a filtered subscription cannot detect a missing log by ordinal gap, so a dropped log is invisible until `state_gate` divergence catches it. Task 1's module doc says so. Phase 2 must not treat I1 as fully discharged.

**Type consistency.** `Ordinal{block,tx_index,log_index}` defined Task 1, used Task 5. `word()` defined Task 2, used Task 3. `V2SyncDelta`/`ClSwapDelta` Tasks 2/3 → Task 5. `TrustState`/`Provenance`/`V2Snapshot`/`ClSnapshot` Task 4 → Task 5. `ApplyOutcome` Task 5 → Task 8. `PoolMonitorKind::ConcentratedLiquidity` Task 6 → used in `pollable_pairs` same task. `divergence_bps` Task 7, unused until Phase 2's anchor task — flagged `#[allow(dead_code)]`.

**Risk flagged in-plan.** Task 6 Step 5 explicitly tests whether the provider accepts a ~750-address subscription, because that is a new and untested condition and the failure mode is silence — the exact thing that cost this project a full debugging cycle.
