# Aerodrome v2 Volatile Pools Implementation Plan (Task 8.5 R24)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Price and settle Aerodrome v2's volatile (x·y=k) pools as a fifth venue in the shadow's two-hop WETH frontier.

**Architecture:** A book entry gains an optional `Reserves`. A constant-product pool is loaded by its own reads (`live/cp.rs`), updated from each `Sync` log, and quoted with the pool's own arithmetic. Every concentrated-liquidity path is unchanged. Hops go through a new `GENERIC` adapter (id 4) bound to Aerodrome's router. The venue is reachable only after the owner registers that adapter.

**Tech Stack:** Rust (`apex-runtime`, `apex-exec`), Python 3 (`scripts/data`), Foundry `cast`.

**Spec:** `docs/superpowers/specs/2026-10-09-aerodrome-v2-venue-design.md`

## Global Constraints

- Factory `0x420DD381b31aEf6683db6B902084cB0FFECe40Da`; router `0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43`; venue id `AERODROME_VOLATILE = VenueId(5)`; adapter id 4; selector `0xcac88ea9`.
- Volatile pools only: a record or pool with `stable` true, or unknown, is refused.
- Quote, in the pool's order and rounding: `in' = in − floor(in · fee_ppm / 1_000_000)` (identical to the pool's `fee_bps / 10_000`, since `fee_ppm = 100 · fee_bps`), then `out = floor(in' · rOut / (rIn + in'))`.
- `Sync(uint256,uint256)` topic `0xcf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a`.
- `InsufficientOutputAmount()` `0x42301c23` → `RevertClass::MinOutNotMet`.
- Never `cargo fmt`. Use `-j 3` for cargo. Stage named paths only. Run all five CI gates before every commit (`$SCRATCH/gates.sh`). Verify each commit in a clean worktree with its own `CARGO_TARGET_DIR`. Never put `BLOCKPI_KEY` in argv. Pass it as `BLOCKPI_KEY=$(grep ^BLOCKPI_KEY= .env | cut -d= -f2-) cast … --rpc-url base` (foundry.toml interpolates it).
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## File Structure

- Create `crates/apex-runtime/src/live/cp.rs`: `Reserves`, `quote_out`, the load reads and their decode, `snapshot`, `fee_ppm_from`.
- Modify `crates/apex-runtime/src/live/inventory.rs`: `Venue::AerodromeV2`, `is_constant_product`, `stable` in the record.
- Modify `crates/apex-runtime/src/live/abi.rs`: selectors `STABLE`, `GET_RESERVES`, `GET_FEE`, `IS_POOL`; `call_address_bool`.
- Modify `crates/apex-runtime/src/live/book.rs`: `PoolSnapshot::reserves`, `Unloadable::NotVolatile`, `SyncWrite`/`StateWrite`/`apply_writes`/`apply_sync`, CP load, reload, and `refresh_cp_fees`.
- Modify `crates/apex-runtime/src/live/pricing.rs`: per-hop dispatch and `max_input`.
- Modify `crates/apex-runtime/src/live/frontier.rs`: adapter constants; no tick neighbourhood for a CP pool.
- Modify `crates/apex-runtime/src/live/feed.rs`: `SYNC`, `SyncLog`, `decode_sync`, `Held`, one-write bursts, `reserve_change_usd`, and `pool_price` in `usd_prices`.
- Modify `crates/apex-runtime/src/live/calls.rs`: binding and step for `AerodromeV2`.
- Modify `crates/apex-runtime/src/live/gas.rs`: `PerVenue::aerodrome_v2` and measured values.
- Modify `crates/apex-runtime/src/live/admission.rs`: `ReserveSync` and `[SYNC]` for a CP venue.
- Modify `crates/apex-runtime/src/live/sim.rs`: `INSUFFICIENT_OUTPUT_AMOUNT`.
- Modify `crates/apex-runtime/src/live/mod.rs`: `pub mod cp;`.
- Modify `crates/apex-runtime/src/shadow/mod.rs`: subscribe to `SYNC`; head-loop fee refresh; `ReadFailures::fees`.
- Modify `scripts/shadow-status.sh`: print `fees`.
- Modify `crates/apex-exec/src/encode/mod.rs`: `AerodromeSwap`, `aerodrome_swap_exact_tokens_for_tokens`, the selector.
- Create `scripts/data/build_aerodrome_v2_pools.py`: writes `data/base/aerodrome_v2/pools.jsonl` (outside git).
- Create `scripts/data/verify_aerodrome_v2.py`: inventory truth, fee parity, quote parity and Sync parity.
- Tests:
  - `crates/apex-exec/tests/generic_step.rs`.
  - `crates/apex-runtime/tests/live_cp.rs` (new).
  - Plus `live_venues.rs`, `live_book.rs`, `live_pricing.rs`, `live_calls.rs`, `live_feed.rs`, `live_gas.rs`, `live_sim.rs` and `shadow_parts.rs`.

---

### Task 1: The router call

**Files:**
- Modify: `crates/apex-exec/src/encode/mod.rs` (after `v3_router_exact_input_single`)
- Test: `crates/apex-exec/tests/generic_step.rs`

**Interfaces:**
- Produces: `pub const AERODROME_SWAP_EXACT_TOKENS_FOR_TOKENS: [u8; 4]`; `pub struct AerodromeSwap { token_in, token_out, factory, recipient: Address, deadline: u64, amount_in: U256, min_out: U256 }`; `pub fn aerodrome_swap_exact_tokens_for_tokens(&AerodromeSwap) -> Result<Vec<u8>, EncodeError>`.

- [ ] **Step 1: Write the failing tests** (append to `generic_step.rs`, and add the three names to its `use apex_exec::encode::{…}`):

```rust
const AERO_FACTORY: Address = address!("420DD381b31aEf6683db6B902084cB0FFECe40Da");

/// `cast calldata "swapExactTokensForTokens(uint256,uint256,(address,address,bool,address)[],address,uint256)"
///  1500000000000000000 4000000000 "[(WETH,USDC,false,AERO_FACTORY)]" EXECUTOR 1790000000`
const AERO_SWAP: &str = "cac88ea900000000000000000000000000000000000000000000000014d1120d7b16000000000000000000000000000000000000000000000000000000000000ee6b280000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000001c3d856d29ea2118c8d955070a6ad83c984586f3000000000000000000000000000000000000000000000000000000006ab13b8000000000000000000000000000000000000000000000000000000000000000010000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda029130000000000000000000000000000000000000000000000000000000000000000000000000000000000000000420dd381b31aef6683db6b902084cb0ffece40da";

/// The same, `1 2 "[(USDC,WETH,false,AERO_FACTORY)]" EXECUTOR 3`: the other direction.
const AERO_BACK: &str = "cac88ea90000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000a00000000000000000000000001c3d856d29ea2118c8d955070a6ad83c984586f300000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000000000000000000000000000001000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda0291300000000000000000000000042000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000000000000000000000000000000420dd381b31aef6683db6b902084cb0ffece40da";

fn aero_swap() -> AerodromeSwap {
    AerodromeSwap {
        token_in: WETH,
        token_out: USDC,
        factory: AERO_FACTORY,
        recipient: EXECUTOR,
        deadline: 1_790_000_000,
        amount_in: U256::from(1_500_000_000_000_000_000u128),
        min_out: U256::from(4_000_000_000u64),
    }
}

#[test]
fn the_aerodrome_selector_is_its_signatures_hash() {
    let sig = "swapExactTokensForTokens(uint256,uint256,(address,address,bool,address)[],address,uint256)";
    assert_eq!(AERODROME_SWAP_EXACT_TOKENS_FOR_TOKENS, keccak256(sig.as_bytes())[..4]);
}

/// One volatile route, held byte-for-byte to `cast`, both directions.
#[test]
fn an_aerodrome_swap_encodes_as_cast_does() {
    assert_eq!(hex::encode(aerodrome_swap_exact_tokens_for_tokens(&aero_swap()).unwrap()), AERO_SWAP);
    let back = AerodromeSwap {
        token_in: USDC,
        token_out: WETH,
        amount_in: U256::from(1u64),
        min_out: U256::from(2u64),
        deadline: 3,
        ..aero_swap()
    };
    assert_eq!(hex::encode(aerodrome_swap_exact_tokens_for_tokens(&back).unwrap()), AERO_BACK);
}

#[test]
fn an_aerodrome_swap_without_a_minimum_is_refused() {
    let unbounded = AerodromeSwap { min_out: U256::ZERO, ..aero_swap() };
    assert_eq!(aerodrome_swap_exact_tokens_for_tokens(&unbounded), Err(EncodeError::ZeroMinOut));
}
```

- [ ] **Step 2: Run, expect a compile failure** (the names don't exist): `cargo test -j 3 -p apex-exec --test generic_step`

- [ ] **Step 3: Implement** (in `encode/mod.rs`, after `v3_router_exact_input_single`):

```rust
/// `swapExactTokensForTokens(uint256,uint256,(address,address,bool,address)[],address,uint256)`:
/// Aerodrome's v2 router, one route (R24).
pub const AERODROME_SWAP_EXACT_TOKENS_FOR_TOKENS: [u8; 4] = [0xca, 0xc8, 0x8e, 0xa9];

/// One hop through Aerodrome's v2 router: a single **volatile** route. The
/// router pulls `amount_in` from the caller by `transferFrom`, so the adapter
/// step approves it, and pays `recipient`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AerodromeSwap {
    pub token_in: Address,
    pub token_out: Address,
    /// The pool factory the route names; the router finds the pool by tokens,
    /// `stable` and factory.
    pub factory: Address,
    pub recipient: Address,
    /// Unix seconds; the plan's deadline, which the executor checks first.
    pub deadline: u64,
    pub amount_in: U256,
    pub min_out: U256,
}

pub fn aerodrome_swap_exact_tokens_for_tokens(s: &AerodromeSwap) -> Result<Vec<u8>, EncodeError> {
    if s.min_out.is_zero() {
        return Err(EncodeError::ZeroMinOut);
    }
    let mut out = Vec::with_capacity(4 + 10 * WORD);
    out.extend_from_slice(&AERODROME_SWAP_EXACT_TOKENS_FOR_TOKENS);
    push_u256(&mut out, s.amount_in);
    push_u256(&mut out, s.min_out);
    // The routes array's offset: past the five head words.
    push_uint(&mut out, (5 * WORD) as u64);
    push_address(&mut out, s.recipient);
    push_uint(&mut out, s.deadline);
    // One route: (from, to, stable, factory), all static, so inline.
    push_uint(&mut out, 1);
    push_address(&mut out, s.token_in);
    push_address(&mut out, s.token_out);
    push_uint(&mut out, 0); // stable: false — volatile pools only
    push_address(&mut out, s.factory);
    Ok(out)
}
```

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 -p apex-exec --test generic_step`

---

### Task 2: The venue's fixed facts

**Files:**
- Modify:
  - `live/inventory.rs`, `live/feed.rs` (`SYNC` and `swap_topic`), `live/gas.rs`, `live/frontier.rs` (constants)
  - `live/calls.rs`, `live/admission.rs`, `live/sim.rs`
- Test:
  - `tests/live_venues.rs`, `tests/live_sim.rs`, `tests/live_gas.rs`, `tests/live_calls.rs`
  - `tests/shadow_parts.rs`, `tests/live_book.rs` (inventory files for the new directory)

**Interfaces:**
- Consumes: Task 1's encoder.
- Produces:
  - `Venue::AerodromeV2` and `Venue::is_constant_product(self) -> bool`.
  - `feed::SYNC: B256`.
  - `frontier::{AERODROME_V2_ADAPTER: u16 = 4, AERODROME_V2_ROUTER: Address}`.
  - `PerVenue::aerodrome_v2: u64`.
  - `sim::revert::INSUFFICIENT_OUTPUT_AMOUNT`.

- [ ] **Step 1: Write the failing tests** (append to `tests/live_venues.rs`; extend its `use` lines with `apex_runtime::live::calls::binding`, `apex_runtime::live::feed::SYNC`, `apex_runtime::live::frontier::{AERODROME_V2_ADAPTER, AERODROME_V2_ROUTER}` and `apex_runtime::live::inventory::{self, UniverseFilter}`):

```rust
#[test]
fn aerodromes_volatile_pools_are_a_venue_of_their_own() {
    let v = Venue::AerodromeV2;
    assert!(Venue::ALL.contains(&v));
    assert_eq!(v.dir(), "aerodrome_v2");
    assert_eq!(v.factory(), address!("420DD381b31aEf6683db6B902084cB0FFECe40Da"));
    assert_eq!(v.id(), venue_ids::AERODROME_VOLATILE);
    assert!(v.fee_is_static(), "no TWAP: the factory sets it, re-read each head");
    assert!(v.is_constant_product());
    assert!(Venue::ALL.iter().filter(|v| v.is_constant_product()).eq([Venue::AerodromeV2].iter()));
}

/// A volatile pool's state arrives in `Sync`, not `Swap`.
#[test]
fn its_state_arrives_in_sync() {
    assert_eq!(swap_topic(Venue::AerodromeV2), SYNC);
    assert_eq!(SYNC, alloy_primitives::keccak256(b"Sync(uint256,uint256)"));
}

/// No ticks: a hop is its fixed figure, crossing nothing.
#[test]
fn it_settles_at_one_figure_a_hop() {
    let aero = HopSteps { venue: Venue::AerodromeV2, zero_for_one: true, crossed: 0, word_steps: 0 };
    let e = gas::MEASURED.estimate(&[aero]);
    assert_eq!(e.expected.0, gas::MEASURED.hop.aerodrome_v2);
    assert_eq!(e.ceiling.0, gas::MEASURED.hop.aerodrome_v2 + gas::MEASURED.hop_margin);
}

#[test]
fn it_binds_adapter_four_at_aerodromes_router() {
    let b = binding(Venue::AerodromeV2).expect("an adapter venue");
    assert_eq!((b.id, b.router), (AERODROME_V2_ADAPTER, AERODROME_V2_ROUTER));
    assert_eq!(b.id, 4);
    assert_eq!(AERODROME_V2_ROUTER, address!("cF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43"));
    assert_eq!(b.selector, [0xca, 0xc8, 0x8e, 0xa9]);
}

/// Only volatile records load: a stable one, or one that does not say, is refused.
#[test]
fn the_inventory_takes_volatile_aerodrome_records_only() {
    let dir = tempfile::tempdir().unwrap();
    for v in Venue::ALL {
        std::fs::create_dir_all(dir.path().join(v.dir())).unwrap();
        std::fs::write(dir.path().join(v.dir()).join("pools.jsonl"), "").unwrap();
    }
    let rec = |n: u8, stable: Option<bool>| {
        let mut r = serde_json::json!({ "pool": format!("0x{n:040x}"), "token0": "0x4200000000000000000000000000000000000006",
            "token1": "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913", "fee": 30, "fee_ppm_onchain": 3000,
            "hub_usd_liquidity": 1e6, "hub_symbol": "WETH" });
        if let Some(s) = stable { r["stable"] = serde_json::json!(s); }
        r.to_string()
    };
    let lines = [rec(1, Some(false)), rec(2, Some(true)), rec(3, None)];
    std::fs::write(dir.path().join("aerodrome_v2").join("pools.jsonl"), lines.join("\n")).unwrap();
    let filter = UniverseFilter { max_fee_ppm: 3_000, min_depth_usd: 100_000.0 };
    let got: Vec<u8> = inventory::load(dir.path(), filter).unwrap().iter().map(|s| s.pool.as_slice()[19]).collect();
    assert_eq!(got, vec![1]);
}
```

Append to `tests/live_sim.rs`'s `the_classification_table`: the pair `(revert::INSUFFICIENT_OUTPUT_AMOUNT, "InsufficientOutputAmount()")` in the selector list, and `(with(revert::INSUFFICIENT_OUTPUT_AMOUNT), RevertClass::MinOutNotMet)` in the class list.

- [ ] **Step 2: Run, expect compile failures:** `cargo test -j 3 -p apex-runtime --test live_venues --test live_sim`

- [ ] **Step 3: Implement.**

`inventory.rs`:
- Add the variant after `SlipstreamV3`, with the doc comment `/// Aerodrome v2's volatile (x·y=k) pools, factory 0x420D… (R24). Stable pools are refused.`
- `ALL: [Self; 5]`, ending `Self::AerodromeV2`.
- `id`: `Self::AerodromeV2 => venue_ids::AERODROME_VOLATILE`.
- `dir`: `"aerodrome_v2"`.
- `factory`: `address!("420DD381b31aEf6683db6B902084cB0FFECe40Da")`. Extend its doc comment: "for Aerodrome v2, its router's `defaultFactory()` (2026-10-09)".
- `fee_is_static`: `matches!(self, Self::UniswapV3 | Self::PancakeV3 | Self::AerodromeV2)`, with the comment "Aerodrome v2's factory sets a pool's fee, which no swap moves; the book re-reads it each head (R24)".
- Add:

```rust
    /// A constant-product venue: reserves and a fee, no ticks (R24).
    pub const fn is_constant_product(self) -> bool {
        matches!(self, Self::AerodromeV2)
    }
```

In `struct Record` add `#[serde(default)] stable: Option<bool>,`. In `load`, at the top of the record's loop body after parsing:

```rust
            // Aerodrome v2: volatile pools only. A stable pool's curve is not
            // the one priced, and a record that does not say is not trusted.
            if venue.is_constant_product() && r.stable != Some(false) {
                continue;
            }
```

`feed.rs`: add `SYNC` beside `BURN`, and extend `swap_topic`:

```rust
/// Aerodrome v2's `Sync(uint256,uint256)`: a volatile pool's reserves after
/// every swap, mint and burn — its whole state (R24).
pub const SYNC: B256 = b256!("cf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a");
```

In `swap_topic`, add the arm `Venue::AerodromeV2 => SYNC,`. Its doc becomes "The log that carries a venue's post-trade state".

`gas.rs`:
- Add `pub aerodrome_v2: u64,` to `PerVenue`, and `Venue::AerodromeV2 => self.aerodrome_v2,` to `of`.
- In `MEASURED`:
  - `hop`: `aerodrome_v2: 266_500`. Its comment: "PancakeSwap's measured hop (both through GENERIC adapters) plus what Aerodrome's router swap used over PancakeSwap's SmartRouter's, 205,780 − 175,239 gas, `eth_simulateV1` of 0.01 WETH → USDC from an override-funded account, 2026-10-09. Verification check 4 re-measures it through the executor."
  - `crossing_down` and `crossing_up`: `aerodrome_v2: 0`.
  - `crossing_ceiling`: `aerodrome_v2: 0`. Its comment: "it crosses no ticks".

`frontier.rs`, after `SLIPSTREAM_V3_ROUTER`:

```rust
/// The adapter id Aerodrome v2's router is registered under (R24).
pub const AERODROME_V2_ADAPTER: u16 = 4;

/// Aerodrome's v2 `Router` on Base: what adapter 4 must be. Its
/// `defaultFactory()` is `0x420D…`, its `weth()` WETH, and its
/// `getAmountsOut` matched the WETH/USDC pool's own quote to the unit
/// (2026-10-09).
pub const AERODROME_V2_ROUTER: Address = address!("cF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43");
```

`calls.rs`:
- Import `AERODROME_V2_ADAPTER` and `AERODROME_V2_ROUTER`, plus `aerodrome_swap_exact_tokens_for_tokens`, `AerodromeSwap` and `AERODROME_SWAP_EXACT_TOKENS_FOR_TOKENS`.
- Add to `binding`:

```rust
        Venue::AerodromeV2 => Some(AdapterBinding {
            id: AERODROME_V2_ADAPTER,
            router: AERODROME_V2_ROUTER,
            selector: AERODROME_SWAP_EXACT_TOKENS_FOR_TOKENS,
        }),
```

and to `step`:

```rust
            Venue::AerodromeV2 => {
                let call = aerodrome_swap_exact_tokens_for_tokens(&AerodromeSwap {
                    token_in: leg.token_in,
                    token_out: leg.token_out,
                    factory: Venue::AerodromeV2.factory(),
                    recipient: k.executor_address,
                    deadline: k.deadline,
                    amount_in,
                    min_out,
                })
                .map_err(|e| refuse(e.to_string()))?;
                Ok(Step { op: Op::Generic, data: generic_step(AERODROME_V2_ADAPTER, leg.token_in, amount_in, &call) })
            }
```

Add a bullet to the module doc's venue list: "**Aerodrome v2**: the `GENERIC` op through adapter 4, calling the router's `swapExactTokensForTokens` with one volatile route; tokens, `stable` and factory identify the pool."

`admission.rs` `record`:

```rust
        reconstruction: Some(if p.spec.venue.is_constant_product() {
            ReconstructionMethod::ReserveSync
        } else {
            ReconstructionMethod::ConcentratedLiquidityLogs
        }),
```

and

```rust
        update_mapping: Some(if p.spec.venue.is_constant_product() {
            vec![SYNC]
        } else {
            vec![swap_topic(p.spec.venue), MINT, BURN]
        }),
```

Import `SYNC`, and add a table row to the module doc: "| reconstruction | concentrated-liquidity logs, or `Sync` for a constant-product pool |".

`sim.rs` `revert`:

```rust
    /// Aerodrome's router: `InsufficientOutputAmount()` — its minimum output.
    pub const INSUFFICIENT_OUTPUT_AMOUNT: [u8; 4] = [0x42, 0x30, 0x1c, 0x23];
```

In `classify`: `INSUFFICIENT_OUTPUT_AMOUNT => RevertClass::MinOutNotMet,`.

Tests that build every venue's inventory directory:
- In `shadow_parts.rs`'s two universe tests, add `("aerodrome_v2", vec![])` to the first test's list, and `"aerodrome_v2"` to the second test's list of empty directories.
- In `live_book.rs`, add the same wherever `write_inventory` is called for every venue.
- In `live_gas.rs`, add `aerodrome_v2: …` to every `PerVenue { … }` literal, with any value: 400_000 for `hop`, 4_000 for crossings, 70_000 for `crossing_ceiling`, and 0 where the others are 0. Add `"aerodrome_v2" => Venue::AerodromeV2,` to its `venue()` match.
- In `live_calls.rs`'s `expected_step`, add an `AerodromeV2` arm. It mirrors `step`, with `EXECUTOR` and `DEADLINE`, and `generic_step(AERODROME_V2_ADAPTER, …)`.

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 -p apex-runtime --test live_venues --test live_sim --test live_gas --test live_calls --test shadow_parts --test live_book`

---

### Task 3: Constant-product state in the book

**Files:**
- Create: `crates/apex-runtime/src/live/cp.rs`; add `pub mod cp;` to `live/mod.rs`
- Modify: `live/abi.rs`, `live/book.rs`
- Test: `tests/live_cp.rs` (new), `tests/live_book.rs`

**Interfaces:**
- Consumes: `Venue::is_constant_product`.
- Produces, in `live::cp`:
  - `Reserves { reserve0, reserve1: ethers U256 }`
  - `quote_out(&Reserves, fee_ppm: u32, amount_in: U256, zero_for_one: bool) -> Option<U256>`
  - `READS: usize`
  - `state_calls(&PoolSpec) -> Vec<(Address, Vec<u8>)>`
  - `decode_state(&PoolSpec, &[Option<Vec<u8>>]) -> Result<Loaded, Unloadable>`
  - `snapshot(spec, Loaded, code_hash, block) -> PoolSnapshot`
  - `fee_ppm_from(&[u8]) -> Option<u32>`
- Produces, in `book`:
  - `PoolSnapshot::reserves: Option<Reserves>` and `Unloadable::NotVolatile`
  - `SyncWrite`, `StateWrite`, `apply_writes`, `apply_sync(pool, U256, U256, LogPosition) -> SwapApplied`
  - `refresh_cp_fees(&ChainReads, block) -> Result<usize, ReadError>`
- Produces, in `abi`: `call_address_bool`.

- [ ] **Step 1: Write the failing tests.** New `tests/live_cp.rs`:

```rust
//! R24: Aerodrome v2's volatile pools, quoted as the pool quotes.

use apex_runtime::live::cp::{quote_out, Reserves};
use ethers_core::types::U256;

/// `getReserves()` and `getAmountOut` of the WETH/USDC pool 0xcDAC0d6c…, block
/// 52,364,894, fee 30 bps; token0 WETH.
const R0: u128 = 1_823_383_892_520_317_644_689;
const R1: u128 = 4_541_987_609_188;
const VECTORS: [(bool, u128, u128); 8] = [
    (true, 10_000_000_000_000_000, 24_834_797),
    (true, 100_000_000_000_000_000, 248_335_749),
    (true, 1_000_000_000_000_000_000, 2_482_136_085),
    (true, 123_456_789_012_345_678, 306_583_410),
    (false, 25_000_000, 10_006_102_620_585_099),
    (false, 250_000_000, 100_056_084_545_959_744),
    (false, 2_500_000_000, 1_000_066_947_794_155_070),
    (false, 1_234_567_891, 493_997_360_556_505_433),
];

fn pool() -> Reserves {
    Reserves { reserve0: U256::from(R0), reserve1: U256::from(R1) }
}

/// **To the unit**, both directions, against the chain.
#[test]
fn the_quote_is_the_pools_get_amount_out_to_the_unit() {
    for (zero_for_one, amount_in, out) in VECTORS {
        assert_eq!(quote_out(&pool(), 3_000, U256::from(amount_in), zero_for_one), Some(U256::from(out)), "{amount_in}");
    }
}

/// The pool takes `floor(in · fee)` off the input; `in · (1 − fee)` floored
/// would keep a unit less, and one of the eight vectors above sees it.
#[test]
fn the_fee_comes_off_the_input_rounded_down() {
    let r = Reserves { reserve0: U256::from(10u64), reserve1: U256::from(1_000u64) };
    // 1 · 0.003 floors to 0, so the whole unit swaps: 1000 · 1 / 11.
    assert_eq!(quote_out(&r, 3_000, U256::one(), true), Some(U256::from(90u64)));
}

#[test]
fn nothing_in_or_an_empty_side_quotes_nothing() {
    assert_eq!(quote_out(&pool(), 3_000, U256::zero(), true), None);
    let empty = Reserves { reserve0: U256::zero(), reserve1: U256::from(R1) };
    assert_eq!(quote_out(&empty, 3_000, U256::from(10u64), true), None);
    assert_eq!(quote_out(&empty, 3_000, U256::from(10u64), false), None);
}
```

Append to `tests/live_book.rs`, in the book section. Import `apex_runtime::live::cp::Reserves`, and `StateWrite, SyncWrite` from `book`:

```rust
const AERO: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");
const AERO_R0: u128 = 1_823_383_892_520_317_644_689;
const AERO_R1: u128 = 4_541_987_609_188;

fn aero_spec() -> PoolSpec {
    PoolSpec { pool: AERO, venue: Venue::AerodromeV2, token0: WETH, token1: USDC, fee_ppm: 3_000, depth_usd: 4_900_000.0 }
}

fn aero_healthy(node: &Scripted) {
    let f = Venue::AerodromeV2.factory();
    node.set(AERO, abi::call0(selector::TOKEN0), wa(WETH));
    node.set(AERO, abi::call0(selector::TOKEN1), wa(USDC));
    node.set(AERO, abi::call0(selector::FACTORY), wa(f));
    node.set(AERO, abi::call0(selector::STABLE), w(U256::ZERO));
    let mut reserves = w(U256::from(AERO_R0));
    reserves.extend(w(U256::from(AERO_R1)));
    reserves.extend(w(U256::from(1_791_000_000u64)));
    node.set(AERO, abi::call0(selector::GET_RESERVES), reserves);
    node.set(WETH, abi::call0(selector::DECIMALS), w(U256::from(18)));
    node.set(USDC, abi::call0(selector::DECIMALS), w(U256::from(6)));
    node.set(f, abi::call_address_bool(selector::GET_FEE, AERO, false), w(U256::from(30)));
    node.set(f, abi::call_address(selector::IS_POOL, AERO), w(U256::from(1)));
}

async fn aero_book(node: Arc<Scripted>) -> (PoolBook, Vec<apex_runtime::live::book::Unloaded>) {
    PoolBook::load(&ChainReads::new(node), &[aero_spec()], 100).await.expect("reads")
}

fn eth(v: u128) -> ethers_core::types::U256 {
    ethers_core::types::U256::from(v)
}

#[tokio::test]
async fn a_volatile_aerodrome_pool_loads_with_its_reserves_and_fee() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    let (book, refused) = aero_book(node).await;
    assert!(refused.is_empty(), "{refused:?}");
    let p = book.get(AERO).expect("loaded");
    assert_eq!(p.reserves, Some(Reserves { reserve0: eth(AERO_R0), reserve1: eth(AERO_R1) }));
    assert_eq!(p.state.fee_ppm, 3_000, "30 bps");
    assert_eq!(p.decimals, (18, 6));
    assert!(p.ladder_covers_price(), "no ladder to leave");
    // Its tick state is empty: a concentrated-liquidity quote of it fails closed.
    assert_eq!(p.state.liquidity, 0);
    assert!(p.state.sqrt_price_x96.is_zero());
}

#[tokio::test]
async fn a_stable_aerodrome_pool_is_refused() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    node.set(AERO, abi::call0(selector::STABLE), w(U256::from(1)));
    let (book, refused) = aero_book(node).await;
    assert!(book.is_empty());
    assert_eq!(refused[0].why, Unloadable::NotVolatile);
}

/// The pool's own `factory()` is not enough: the factory must know it.
#[tokio::test]
async fn an_aerodrome_pool_its_factory_does_not_know_is_refused() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    node.set(Venue::AerodromeV2.factory(), abi::call_address(selector::IS_POOL, AERO), w(U256::ZERO));
    let (book, refused) = aero_book(node).await;
    assert!(book.is_empty());
    assert!(matches!(refused[0].why, Unloadable::WrongFactory { .. }), "{refused:?}");
}

/// A `Sync` replaces the reserves outright; an older one is refused; a write
/// of the other kind of state is refused, whichever way round.
#[tokio::test]
async fn a_sync_replaces_the_reserves_and_an_older_one_is_refused() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    healthy(&node, Venue::UniswapV3.factory());
    let (book, _) = PoolBook::load(&ChainReads::new(node), &[aero_spec(), spec()], 100).await.unwrap();
    let (a, b) = (U256::from(5u64), U256::from(7u64));
    assert_eq!(book.apply_sync(AERO, a, b, (101, 2)), SwapApplied::Updated);
    assert_eq!(book.get(AERO).unwrap().reserves, Some(Reserves { reserve0: eth(5), reserve1: eth(7) }));
    assert_eq!(book.apply_sync(AERO, b, a, (101, 1)), SwapApplied::Stale);
    assert_eq!(book.apply_sync(AERO, b, a, (100, 9)), SwapApplied::Stale, "already in the read");
    assert_eq!(book.apply_sync(WETH_USDC, a, b, (102, 0)), SwapApplied::Unknown, "not a reserve pool");
    assert_eq!(book.apply_swap(AERO, U256::from(1u64) << 96, 1, 0, (102, 1)), SwapApplied::Unknown, "not a tick pool");
    // Both kinds in one write, in order.
    let writes = [
        StateWrite::Sync(SyncWrite { pool: AERO, reserve0: b, reserve1: a, at: (103, 0) }),
        StateWrite::Swap(SwapWrite { pool: WETH_USDC, sqrt_price_x96: U256::from(1u64) << 96, liquidity: 9, tick: TICK as i32, at: (103, 1) }),
    ];
    assert_eq!(book.apply_writes(&writes), vec![SwapApplied::Updated, SwapApplied::Updated]);
}

/// A reload never rolls back a newer `Sync`.
#[tokio::test]
async fn a_reload_keeps_a_newer_sync() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    let reads = ChainReads::new(node.clone());
    let (book, _) = PoolBook::load(&reads, &[aero_spec()], 100).await.unwrap();
    book.apply_sync(AERO, U256::from(5u64), U256::from(7u64), (101, 0));
    book.reload(&reads, &[AERO], 100).await.unwrap();
    assert_eq!(book.get(AERO).unwrap().reserves, Some(Reserves { reserve0: eth(5), reserve1: eth(7) }));
}

/// The factory's fee manager can change a pool's fee; each head re-reads it.
#[tokio::test]
async fn a_fee_refresh_follows_the_factory() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    let reads = ChainReads::new(node.clone());
    let (book, _) = PoolBook::load(&reads, &[aero_spec()], 100).await.unwrap();
    assert_eq!(book.refresh_cp_fees(&reads, 101).await.unwrap(), 0, "unchanged");
    node.set(Venue::AerodromeV2.factory(), abi::call_address_bool(selector::GET_FEE, AERO, false), w(U256::from(50)));
    assert_eq!(book.refresh_cp_fees(&reads, 102).await.unwrap(), 1);
    assert_eq!(book.get(AERO).unwrap().state.fee_ppm, 5_000);
}
```

Add to `every_selector_is_its_signatures_hash`: `(selector::STABLE, "stable()")`, `(selector::GET_RESERVES, "getReserves()")`, `(selector::GET_FEE, "getFee(address,bool)")`, `(selector::IS_POOL, "isPool(address)")`. Add this test too:

```rust
/// `cast calldata "getFee(address,bool)" 0xcdac0d6c6c59727a65f871236188350531885c43 false`
#[test]
fn get_fee_encodes_as_cast_does() {
    assert_eq!(
        hex::encode(abi::call_address_bool(selector::GET_FEE, AERO, false)),
        "cc56b2c5000000000000000000000000cdac0d6c6c59727a65f871236188350531885c430000000000000000000000000000000000000000000000000000000000000000"
    );
}
```

Every `PoolSnapshot { … }` literal in the tests gains `reserves: None,`: `live_fees.rs`, `live_admission.rs`, `live_calls.rs`, `live_feed.rs`, `live_reader.rs` and `live_pricing.rs`.

- [ ] **Step 2: Run, expect compile failures:** `cargo test -j 3 -p apex-runtime --test live_cp --test live_book`

- [ ] **Step 3: Implement.**

`abi.rs` selectors:

```rust
    /// `stable()` — Aerodrome v2's curve flag.
    pub const STABLE: [u8; 4] = [0x22, 0xbe, 0x3d, 0xe1];
    /// `getReserves()` — Aerodrome v2: `(uint256, uint256, uint256)`.
    pub const GET_RESERVES: [u8; 4] = [0x09, 0x02, 0xf1, 0xac];
    /// `getFee(address,bool)` — Aerodrome v2's factory, in basis points.
    pub const GET_FEE: [u8; 4] = [0xcc, 0x56, 0xb2, 0xc5];
    /// `isPool(address)` — Aerodrome v2's factory.
    pub const IS_POOL: [u8; 4] = [0x5b, 0x16, 0xeb, 0xb7];
```

and the helper:

```rust
/// `f(address,bool)`.
pub fn call_address_bool(sel: [u8; 4], a: Address, b: bool) -> Vec<u8> {
    let mut out = call_address(sel, a);
    out.extend_from_slice(&[0u8; 31]);
    out.push(u8::from(b));
    out
}
```

`book.rs`:
- Add the field `pub reserves: Option<crate::live::cp::Reserves>,` to `PoolSnapshot`, after `dynamic_fee`. Doc: "A constant-product pool's reserves (R24): its whole state with its fee, `state.fee_ppm`. `None` for a concentrated-liquidity pool. A constant-product pool's tick state is empty — zero price and liquidity, no balances, no ladder — so a concentrated-liquidity path that missed the dispatch fails closed."
- `ladder_covers_price`: `self.reserves.is_some() || self.ladder.covers(self.state.tick)`.
- Add `NotVolatile` to `Unloadable`, with the doc "An Aerodrome v2 pool on the stable curve, which is not the one priced."
- Add `reserves: None,` to the snapshot `read` builds.
- In `read`, partition the specs before the CL multicall:

```rust
        let (cp_specs, specs): (Vec<PoolSpec>, Vec<PoolSpec>) =
            specs.iter().cloned().partition(|s| s.venue.is_constant_product());
        let specs = &specs[..];
```

  Then the existing CL body runs over `specs`. Before `Ok((pools, refused))`:

```rust
        // Constant-product pools: their own reads, no ladder, no fee module.
        if !cp_specs.is_empty() {
            let calls: Vec<(Address, Vec<u8>)> = cp_specs.iter().flat_map(crate::live::cp::state_calls).collect();
            let answers = reads.multicall(&calls, block).await?;
            for (spec, a) in cp_specs.iter().zip(answers.chunks(crate::live::cp::READS)) {
                match crate::live::cp::decode_state(spec, a) {
                    Ok(loaded) => {
                        let code_hash = reads.code_hash(spec.pool, block).await?;
                        pools.insert(spec.pool, crate::live::cp::snapshot(spec, loaded, code_hash, block));
                    }
                    Err(why) => refused.push(Unloaded { pool: spec.pool, why }),
                }
            }
        }
```

- Writes:

```rust
/// One `Sync` log's reserves, as [`PoolBook::apply_writes`] writes them (R24).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncWrite {
    pub pool: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    pub at: LogPosition,
}

/// A log's post-trade state, of either kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateWrite {
    Swap(SwapWrite),
    Sync(SyncWrite),
}
```

  Replace `apply_swaps` with a wrapper over `apply_writes`, and add `apply_sync`:

```rust
    pub fn apply_swaps(&self, swaps: &[SwapWrite]) -> Vec<SwapApplied> {
        self.apply_writes(&swaps.iter().map(|s| StateWrite::Swap(*s)).collect::<Vec<_>>())
    }

    /// Several logs' states, swaps and syncs, in order, as **one** write: a
    /// reader sees the book before all of them or after all of them.
    pub fn apply_writes(&self, writes: &[StateWrite]) -> Vec<SwapApplied> {
        self.write(|map, w, status| {
            let applied = writes
                .iter()
                .map(|s| match s {
                    StateWrite::Swap(s) => Self::swap_into(map, w.seq, s),
                    StateWrite::Sync(s) => Self::sync_into(map, w.seq, s),
                })
                .collect();
            (applied, status)
        })
    }

    /// Apply a `Sync` log's reserves, if newer than what the pool holds.
    pub fn apply_sync(&self, pool: Address, reserve0: U256, reserve1: U256, at: LogPosition) -> SwapApplied {
        self.apply_writes(&[StateWrite::Sync(SyncWrite { pool, reserve0, reserve1, at })])[0]
    }

    fn is_newer(old: &PoolSnapshot, at: LogPosition) -> bool {
        match old.last_log {
            Some(last) => at > last,
            None => at.0 > old.block,
        }
    }

    fn sync_into(map: &mut BTreeMap<Address, Arc<PoolSnapshot>>, seq: u64, s: &SyncWrite) -> SwapApplied {
        let Some(old) = map.get(&s.pool).filter(|o| o.reserves.is_some()) else { return SwapApplied::Unknown };
        if !Self::is_newer(old, s.at) {
            return SwapApplied::Stale;
        }
        let mut next = (**old).clone();
        next.reserves = Some(crate::live::cp::Reserves {
            reserve0: u256_to_ethers(s.reserve0),
            reserve1: u256_to_ethers(s.reserve1),
        });
        next.block = s.at.0;
        next.last_log = Some(s.at);
        next.seq = seq;
        map.insert(s.pool, Arc::new(next));
        SwapApplied::Updated
    }
```

  In `swap_into`:
  - Replace `map.get(&s.pool)` with `map.get(&s.pool).filter(|o| o.reserves.is_none())`.
  - Replace its `newer` match with `Self::is_newer(old, s.at)`.
- In `reload`'s keep-the-newer-swap branch, add `snap.reserves = old.reserves;` beside `snap.last_log = old.last_log;`.
- Fee refresh, after `refresh_twaps`:

```rust
    /// Re-read each constant-product pool's fee at `block` (R24): the
    /// factory's fee manager can set one per pool. Returns how many changed.
    /// A pool whose fee does not decode keeps the one it has.
    pub async fn refresh_cp_fees(&self, reads: &ChainReads, block: u64) -> Result<usize, ReadError> {
        let snap = self.snapshot();
        let targets: Vec<(Address, Address)> =
            snap.values().filter(|p| p.reserves.is_some()).map(|p| (p.spec.pool, p.spec.venue.factory())).collect();
        if targets.is_empty() {
            return Ok(0);
        }
        let calls: Vec<(Address, Vec<u8>)> =
            targets.iter().map(|(p, f)| (*f, abi::call_address_bool(selector::GET_FEE, *p, false))).collect();
        let answers = reads.multicall(&calls, block).await?;
        Ok(self.write(|map, w, status| {
            let mut changed = 0;
            for ((pool, _), a) in targets.iter().zip(answers) {
                let Some(fee) = a.as_deref().and_then(crate::live::cp::fee_ppm_from) else { continue };
                let Some(old) = map.get(pool).filter(|o| o.state.fee_ppm != fee) else { continue };
                let mut next = (**old).clone();
                next.state.fee_ppm = fee;
                next.seq = w.seq;
                map.insert(*pool, Arc::new(next));
                changed += 1;
            }
            (changed, status)
        }))
    }
```

New `live/cp.rs`:

```rust
//! Aerodrome v2's volatile pools: x·y=k state and its quote (Task 8.5 R24).
//!
//! # The whole state is two reserves and a fee
//!
//! A volatile pool's `Sync(reserve0, reserve1)` follows every swap, mint and
//! burn, so a pool is its last `Sync` and the fee its factory sets. The fee is
//! `getFee(pool, false)` in basis points, held as `state.fee_ppm` (× 100), and
//! re-read each head: the factory's fee manager can set one per pool.
//!
//! # Quoted as the pool quotes
//!
//! `Pool.getAmountOut`: `in -= in · fee / 10_000`, then `in · rOut / (rIn + in)`,
//! both floored. Checked to the unit against eight `getAmountOut` answers of the
//! WETH/USDC pool `0xcDAC0d6c…` at block 52,364,894 (`tests/live_cp.rs`).
//! `apex_math::quote_common::apply_swap_fee` floors `in · (1 − fee)` instead,
//! one unit less where `in · fee` is not whole, and one of those eight sees it.

use crate::live::abi::{self, selector};
use crate::live::book::{PoolSnapshot, Unloadable};
use crate::live::inventory::PoolSpec;
use alloy_primitives::{Address, B256};
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use ethers_core::types::U256;

/// A volatile pool's reserves, as its last `Sync` or `getReserves()` stated them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reserves {
    pub reserve0: U256,
    pub reserve1: U256,
}

/// What the pool pays for `amount_in`, exactly as `getAmountOut` computes it.
/// `None` for nothing in, an empty side, nothing out, or arithmetic that would
/// overflow — never a guess.
pub fn quote_out(r: &Reserves, fee_ppm: u32, amount_in: U256, zero_for_one: bool) -> Option<U256> {
    let (r_in, r_out) = if zero_for_one { (r.reserve0, r.reserve1) } else { (r.reserve1, r.reserve0) };
    if amount_in.is_zero() || r_in.is_zero() || r_out.is_zero() {
        return None;
    }
    let fee = amount_in.checked_mul(U256::from(fee_ppm))? / U256::from(1_000_000u64);
    let after_fee = amount_in.checked_sub(fee)?;
    let out = after_fee.checked_mul(r_out)? / r_in.checked_add(after_fee)?;
    (!out.is_zero()).then_some(out)
}

/// The reads one pool's state takes, in the order they are decoded.
pub const READS: usize = 9;

pub fn state_calls(s: &PoolSpec) -> Vec<(Address, Vec<u8>)> {
    let factory = s.venue.factory();
    vec![
        (s.pool, abi::call0(selector::TOKEN0)),
        (s.pool, abi::call0(selector::TOKEN1)),
        (s.pool, abi::call0(selector::FACTORY)),
        (s.pool, abi::call0(selector::STABLE)),
        (s.pool, abi::call0(selector::GET_RESERVES)),
        (s.token0, abi::call0(selector::DECIMALS)),
        (s.token1, abi::call0(selector::DECIMALS)),
        (factory, abi::call_address_bool(selector::GET_FEE, s.pool, false)),
        (factory, abi::call_address(selector::IS_POOL, s.pool)),
    ]
}

/// One pool, as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub reserves: Reserves,
    pub fee_ppm: u32,
    pub decimals: (u8, u8),
    pub factory: Address,
}

/// A fee answer, basis points, as ppm.
pub fn fee_ppm_from(answer: &[u8]) -> Option<u32> {
    abi::word_uint(answer, 0, 32).and_then(|bps| u32::try_from(bps.checked_mul(100)?).ok())
}

pub fn decode_state(s: &PoolSpec, a: &[Option<Vec<u8>>]) -> Result<Loaded, Unloadable> {
    let get = |i: usize, read: &'static str| a[i].as_deref().ok_or(Unloadable::Unreadable { read });
    let token0 = abi::word_address(get(0, "token0")?, 0).ok_or(Unloadable::Unreadable { read: "token0" })?;
    let token1 = abi::word_address(get(1, "token1")?, 0).ok_or(Unloadable::Unreadable { read: "token1" })?;
    if (token0, token1) != (s.token0, s.token1) {
        return Err(Unloadable::WrongTokens { chain: (token0, token1) });
    }
    let factory = abi::word_address(get(2, "factory")?, 0).ok_or(Unloadable::Unreadable { read: "factory" })?;
    let known = abi::word_uint(get(8, "isPool")?, 0, 1).ok_or(Unloadable::Unreadable { read: "isPool" })?;
    if factory != s.venue.factory() || known != 1 {
        return Err(Unloadable::WrongFactory { factory });
    }
    if abi::word_uint(get(3, "stable")?, 0, 1).ok_or(Unloadable::Unreadable { read: "stable" })? != 0 {
        return Err(Unloadable::NotVolatile);
    }
    let reserves = get(4, "getReserves")?;
    let word = |i| abi::word_u256(reserves, i).map(apex_types::compat::u256_to_ethers);
    let reserves = Reserves {
        reserve0: word(0).ok_or(Unloadable::Unreadable { read: "getReserves" })?,
        reserve1: word(1).ok_or(Unloadable::Unreadable { read: "getReserves" })?,
    };
    if reserves.reserve0.is_zero() || reserves.reserve1.is_zero() {
        return Err(Unloadable::NoLiquidity);
    }
    let d0 = abi::word_uint(get(5, "decimals")?, 0, 8).ok_or(Unloadable::Unreadable { read: "decimals" })?;
    let d1 = abi::word_uint(get(6, "decimals")?, 0, 8).ok_or(Unloadable::Unreadable { read: "decimals" })?;
    let fee_ppm = fee_ppm_from(get(7, "getFee")?).ok_or(Unloadable::Unreadable { read: "getFee" })?;
    Ok(Loaded { reserves, fee_ppm, decimals: (d0 as u8, d1 as u8), factory })
}

/// A constant-product pool's book entry: reserves and fee, and an **empty**
/// tick state — zero price and liquidity, no balances, no ladder — so a
/// concentrated-liquidity path that missed the dispatch fails closed.
pub fn snapshot(spec: &PoolSpec, l: Loaded, code_hash: B256, block: u64) -> PoolSnapshot {
    PoolSnapshot {
        spec: spec.clone(),
        state: ClPoolState {
            sqrt_price_x96: U256::zero(),
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            fee_ppm: l.fee_ppm,
            balance0: None,
            balance1: None,
        },
        ladder: TickLadder::new(Vec::new(), 0, 0),
        decimals: l.decimals,
        factory: l.factory,
        code_hash,
        block,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
        reserves: Some(l.reserves),
    }
}
```

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 -p apex-runtime --test live_cp --test live_book`, then the whole crate: `cargo test -j 3 -p apex-runtime`

---

### Task 4: Pricing and the frontier

**Files:**
- Modify: `live/pricing.rs` (`LiveCycle::quote`, `max_input`), `live/frontier.rs` (`template`)
- Test: `tests/live_pricing.rs`, `tests/live_calls.rs`

**Interfaces:**
- Consumes: `cp::{quote_out, Reserves, snapshot, Loaded}`, `PoolSnapshot::reserves`.

- [ ] **Step 1: Write the failing tests.** Append to `tests/live_pricing.rs`, using its existing `pool(addr, venue, tick, fee_ppm, l)` helper for the concentrated-liquidity side:

```rust
const AERO: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");

/// A volatile pool beside a Uniswap pool of the same pair: WETH/USDC.
fn mixed(uni_tick: i32, r1: u128) -> PoolBook {
    use apex_runtime::live::cp::{self, Loaded, Reserves};
    let reserves = Reserves {
        reserve0: ethers_core::types::U256::from(1_823_383_892_520_317_644_689u128),
        reserve1: ethers_core::types::U256::from(r1),
    };
    let spec = PoolSpec { pool: AERO, venue: Venue::AerodromeV2, token0: WETH, token1: USDC, fee_ppm: 3_000, depth_usd: 4.9e6 };
    let aero = cp::snapshot(&spec, Loaded { reserves, fee_ppm: 3_000, decimals: (18, 6), factory: Venue::AerodromeV2.factory() }, B256::ZERO, 100);
    PoolBook::from_snapshots([pool(UNI, Venue::UniswapV3, uni_tick, 500, L), aero], ReconstructionStatus::Verified)
}

/// Each hop is quoted by its own pool's model: the volatile hop by the pool's
/// arithmetic, crossing nothing; the Uniswap hop by its ladder.
#[test]
fn a_cycle_through_a_volatile_pool_quotes_each_hop_by_its_own_model() {
    use apex_runtime::live::cp::quote_out;
    let b = mixed(-197_350, 4_541_987_609_188);
    let snap = b.snapshot();
    let cycle = frontier::cycles(BASE, WETH, &snap).into_iter().find(|c| c.legs[0].pool == AERO).expect("a cycle");
    let live = LiveCycle::new(&cycle, &snap).expect("priceable");
    let input = ethers_core::types::U256::from(10u128.pow(17));
    let q = live.quote(input).expect("fills");
    let aero = b.get(AERO).unwrap();
    assert_eq!(Some(q.outputs[0]), quote_out(&aero.reserves.unwrap(), 3_000, input, true));
    assert_eq!((q.hops[0].venue, q.hops[0].crossed, q.hops[0].word_steps), (Venue::AerodromeV2, 0, 0));
    assert_eq!(q.hops[1].venue, Venue::UniswapV3);
}

/// Paying the start token out, a volatile pool can pay no more than its reserve of it.
#[test]
fn a_volatile_pool_paying_weth_bounds_the_input_by_its_weth_reserve() {
    let b = mixed(-197_350, 4_541_987_609_188);
    let snap = b.snapshot();
    let cycle = frontier::cycles(BASE, WETH, &snap).into_iter().find(|c| c.legs[1].pool == AERO).expect("a cycle");
    let live = LiveCycle::new(&cycle, &snap).expect("priceable");
    assert_eq!(live.max_input(), ethers_core::types::U256::from(1_823_383_892_520_317_644_689u128));
}

/// A constant-product pool has no tick neighbourhood to declare.
#[test]
fn a_volatile_pool_declares_no_tick_neighbourhood() {
    let b = mixed(-197_350, 4_541_987_609_188);
    let snap = b.snapshot();
    let cycle = frontier::cycles(BASE, WETH, &snap).into_iter().find(|c| c.legs[0].pool == AERO).unwrap();
    let t = frontier::template(BASE, &cycle, &snap);
    let pools: Vec<Address> = t.tick_neighborhood.keys().map(|p| p.address).collect();
    assert_eq!(pools, vec![UNI]);
}
```

(If `live_pricing.rs` lacks any of the constants `BASE`, `UNI`, `L`, `WETH` or `USDC`, or the imports `frontier`, `PoolSpec`, `B256` or `ReconstructionStatus`, add them with the values `live_calls.rs` uses.)

Append to `tests/live_calls.rs`:

```rust
const AERO: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");

fn aero(r1: u128) -> PoolSnapshot {
    use apex_runtime::live::cp::{self, Loaded, Reserves};
    let reserves = Reserves {
        reserve0: ethers_core::types::U256::from(1_823_383_892_520_317_644_689u128),
        reserve1: ethers_core::types::U256::from(r1),
    };
    let spec = PoolSpec { pool: AERO, venue: Venue::AerodromeV2, token0: WETH, token1: USDC, fee_ppm: 3_000, depth_usd: 4.9e6 };
    cp::snapshot(&spec, Loaded { reserves, fee_ppm: 3_000, decimals: (18, 6), factory: Venue::AerodromeV2.factory() }, B256::ZERO, 100)
}

/// R24: a volatile hop is adapter 4's router call, in either venue order.
/// Uniswap's price is moved either side of the Aerodrome pool's 2,491 USDC.
#[test]
fn an_aerodrome_hop_is_adapter_fours_router_call() {
    for uni in [-196_400, -197_200] {
        let b = Arc::new(PoolBook::from_snapshots(
            [pool(UNI, Venue::UniswapV3, uni, 500, 10), aero(4_541_987_609_188)],
            ReconstructionStatus::Verified,
        ));
        let (cycle, c) = priced(&b);
        let call = LiveCallBuilder::new(b.clone(), [cycle.clone()]).build(&c, &commitment(&c, 1)).expect("builds");
        let input = c.input_amount.get();
        let mid = u256_to_alloy(
            LiveCycle::new(&cycle, &b.snapshot()).unwrap().hop_outputs(u256_to_ethers(input)).unwrap()[0],
        );
        let floor = (input + U256::from(1u64)).max(c.expected_output * U256::from(9_970u64) / U256::from(10_000u64));
        let [l0, l1] = &cycle.legs;
        let want = [
            expected_step(&b, l0.pool, l0.token_in, l0.token_out, input, mid),
            expected_step(&b, l1.pool, l1.token_in, l1.token_out, mid, floor),
        ];
        for (got, (op, data)) in call.plan().steps.iter().zip(want) {
            assert_eq!((got.op, &got.data), (op, &data));
        }
        let aero_step = cycle.legs.iter().position(|l| l.pool == AERO).unwrap();
        assert_eq!(call.plan().steps[aero_step].op, Op::Generic);
    }
}
```

(The ticks: −196,400 prices WETH above the Aerodrome pool and −197,200 below it, so `priced` finds a paying direction both times. If `priced` finds none for a tick, move it further from the Aerodrome price, which is about tick −196,970.)

- [ ] **Step 2: Run, expect FAIL** (the volatile hop is quoted as a tick pool and fails closed): `cargo test -j 3 -p apex-runtime --test live_pricing --test live_calls`

- [ ] **Step 3: Implement.** In `pricing.rs` `LiveCycle::quote`, replace the loop body:

```rust
        for (i, (pool, zero_for_one)) in self.legs.iter().enumerate() {
            // Each pool by its own model (R24): reserves, or a ladder.
            let (out, crossed, word_steps) = match &pool.reserves {
                Some(r) => (crate::live::cp::quote_out(r, pool.state.fee_ppm, amount, *zero_for_one)?, 0, 0),
                None => {
                    let q = quote_exact_input_multi_tick(&pool.state, &pool.ladder, amount, *zero_for_one, MAX_TICKS)?;
                    // A partial fill is not a price for the whole input.
                    if q.exhausted || q.amount_in_consumed < amount {
                        return None;
                    }
                    (q.amount_out, q.ticks_crossed, q.word_steps)
                }
            };
            amount = out;
            outputs[i] = amount;
            hops[i] = Some(HopSteps { venue: pool.spec.venue, zero_for_one: *zero_for_one, crossed, word_steps });
        }
```

`max_input`:

```rust
        let held = match &pool.reserves {
            // A volatile pool holds exactly its reserves.
            Some(r) => Some(if *zero_for_one { r.reserve1 } else { r.reserve0 }),
            None => if *zero_for_one { pool.state.balance1 } else { pool.state.balance0 },
        };
        held.unwrap_or_default()
```

Extend the module doc: "A constant-product hop (R24) is the pool's own `getAmountOut` arithmetic (`live::cp`), and crosses nothing."

In `frontier.rs` `template`, `tick_neighborhood`: change `snapshot.get(&l.pool).map(|p| …)` to `snapshot.get(&l.pool).filter(|p| p.reserves.is_none()).map(|p| …)`.

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 -p apex-runtime --test live_pricing --test live_calls`

---

### Task 5: The feed, the shadow and the report

**Files:**
- Modify: `live/feed.rs`, `shadow/mod.rs`, `scripts/shadow-status.sh`
- Test: `tests/live_feed.rs`

**Interfaces:**
- Consumes: `book::{StateWrite, SyncWrite}`, `PoolSnapshot::reserves`, `PoolBook::refresh_cp_fees`.
- Produces:
  - `feed::SyncLog` and `feed::decode_sync(&RawLog) -> Option<SyncLog>`
  - `feed::Held`
  - `feed::reserve_change_usd(before, after, tokens, decimals, prices) -> Option<f64>`
  - `feed::pool_price(&PoolSnapshot) -> Option<f64>`
  - `ReadFailures::fees`

- [ ] **Step 1: Write the failing tests.** Append to `tests/live_feed.rs`. Its existing helpers build a book and `RawLog`s; add a `sync_log` helper next to them:

```rust
const AERO: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");

fn sync_log(r0: u128, r1: u128, block: u64, index: u64) -> RawLog {
    let mut data = U256::from(r0).to_be_bytes::<32>().to_vec();
    data.extend(U256::from(r1).to_be_bytes::<32>());
    RawLog {
        address: AERO,
        topics: vec![SYNC],
        data,
        block_number: Some(block),
        log_index: Some(index),
        transaction_hash: Some(B256::repeat_byte(index as u8)),
        removed: false,
        pending: true,
    }
}

fn aero_book() -> PoolBook {
    use apex_runtime::live::cp::{self, Loaded, Reserves};
    let spec = PoolSpec { pool: AERO, venue: Venue::AerodromeV2, token0: WETH, token1: USDC, fee_ppm: 3_000, depth_usd: 4.9e6 };
    let r = Reserves {
        reserve0: ethers_core::types::U256::from(1_000u128 * 10u128.pow(18)),
        reserve1: ethers_core::types::U256::from(2_500_000u128 * 10u128.pow(6)),
    };
    PoolBook::from_snapshots(
        [cp::snapshot(&spec, Loaded { reserves: r, fee_ppm: 3_000, decimals: (18, 6), factory: Venue::AerodromeV2.factory() }, B256::ZERO, 100)],
        ReconstructionStatus::Verified,
    )
}

#[test]
fn a_sync_decodes_to_its_reserves() {
    let s = decode_sync(&sync_log(5, 7, 101, 3)).expect("decodes");
    assert_eq!((s.pool, s.reserve0, s.reserve1, s.block, s.log_index), (AERO, U256::from(5u64), U256::from(7u64), 101, 3));
    let mut short = sync_log(5, 7, 101, 3);
    short.data.truncate(32);
    assert!(decode_sync(&short).is_none());
}

/// A burst's `Sync` is applied with the burst, in one write, and the event's
/// notional is the reserve change priced: 10 WETH in, at $2,500 a WETH.
#[test]
fn a_sync_is_applied_with_its_burst_and_sized_by_its_reserve_change() {
    let book = aero_book();
    let mut h = FeedHandler::new(ChainId(8453));
    let r0 = 1_010u128 * 10u128.pow(18);
    let r1 = 2_475_247_524_752u128; // 2,500,000 USDC · 1000 / 1010
    assert!(h.handle(&book, Notification::Log(sync_log(r0, r1, 101, 0)), UnixNanos(1)).is_empty());
    let effects = h.flush(&book);
    let p = book.get(AERO).unwrap();
    assert_eq!(p.reserves.unwrap().reserve0, ethers_core::types::U256::from(r0));
    let Some(Effect::Event(e)) = effects.into_iter().find(|e| matches!(e, Effect::Event(_))) else { panic!("no event") };
    let EventKind::PendingSwap { pools, notional_usd, .. } = e.kind else { panic!("not a swap event") };
    assert_eq!(pools.iter().map(|p| p.address).collect::<Vec<_>>(), vec![AERO]);
    let n = notional_usd.expect("sized");
    assert!((n - 25_000.0).abs() < 300.0, "{n}");
}

#[test]
fn reserve_change_usd_takes_the_larger_priced_side() {
    let prices: BTreeMap<Address, f64> = [(USDC, 1.0)].into_iter().collect();
    let n = reserve_change_usd(
        (U256::from(10u128.pow(18)), U256::from(3_000_000_000u64)),
        (U256::from(2u128 * 10u128.pow(18)), U256::from(500_000_000u64)),
        (WETH, USDC),
        (18, 6),
        &prices,
    );
    assert_eq!(n, Some(2_500.0), "only USDC is priced: 2,500 USDC out");
}
```

(Add any of these imports `live_feed.rs` lacks: `SYNC`, `decode_sync`, `reserve_change_usd`, `Effect`, `FeedHandler`, `EventKind`, `ChainId`, `UnixNanos`, `Notification`, `RawLog`, `PoolSpec`, `Venue`, `BTreeMap`, `B256`, `ReconstructionStatus`.)

- [ ] **Step 2: Run, expect compile failures:** `cargo test -j 3 -p apex-runtime --test live_feed`

- [ ] **Step 3: Implement** in `feed.rs`:

```rust
/// One decoded `Sync` (R24).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncLog {
    pub pool: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    pub block: u64,
    pub log_index: u64,
    pub tx: B256,
    pub pending: bool,
}

/// `None` for anything that is not a well-formed `Sync` with a position.
pub fn decode_sync(l: &RawLog) -> Option<SyncLog> {
    if l.topics.first() != Some(&SYNC) || l.data.len() != 64 {
        return None;
    }
    Some(SyncLog {
        pool: l.address,
        reserve0: abi::word_u256(&l.data, 0)?,
        reserve1: abi::word_u256(&l.data, 1)?,
        block: l.block_number?,
        log_index: l.log_index?,
        tx: l.transaction_hash?,
        pending: l.pending,
    })
}

/// A log the feed holds until its burst is over: a swap's post-swap price, or
/// a volatile pool's reserves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Held {
    Swap(SwapLog),
    Sync(SyncLog),
}

impl Held {
    pub fn pool(&self) -> Address {
        match self { Self::Swap(s) => s.pool, Self::Sync(s) => s.pool }
    }
    pub fn block(&self) -> u64 {
        match self { Self::Swap(s) => s.block, Self::Sync(s) => s.block }
    }
    pub fn log_index(&self) -> u64 {
        match self { Self::Swap(s) => s.log_index, Self::Sync(s) => s.log_index }
    }
    pub fn tx(&self) -> B256 {
        match self { Self::Swap(s) => s.tx, Self::Sync(s) => s.tx }
    }
    pub fn pending(&self) -> bool {
        match self { Self::Swap(s) => s.pending, Self::Sync(s) => s.pending }
    }
    fn write(&self) -> StateWrite {
        match self {
            Self::Swap(s) => StateWrite::Swap(SwapWrite {
                pool: s.pool,
                sqrt_price_x96: s.sqrt_price_x96,
                liquidity: s.liquidity,
                tick: s.tick,
                at: (s.block, s.log_index),
            }),
            Self::Sync(s) => StateWrite::Sync(SyncWrite {
                pool: s.pool,
                reserve0: s.reserve0,
                reserve1: s.reserve1,
                at: (s.block, s.log_index),
            }),
        }
    }
}

/// A pool's price, token1 per token0 in whole units: from its reserves, or its
/// square-root price.
pub fn pool_price(p: &PoolSnapshot) -> Option<f64> {
    let raw = match &p.reserves {
        Some(r) => u256_f64(r.reserve1) / u256_f64(r.reserve0),
        None => {
            let sqrt = u256_f64(p.state.sqrt_price_x96) / 2f64.powi(96);
            sqrt * sqrt
        }
    };
    let v = raw * 10f64.powi(i32::from(p.decimals.0) - i32::from(p.decimals.1));
    (v.is_finite() && v > 0.0).then_some(v)
}

/// A `Sync`'s size in USD: the change in either reserve, whichever side can be
/// priced, the larger if both can. The net of what the trade put in and took out.
pub fn reserve_change_usd(
    before: (U256, U256),
    after: (U256, U256),
    tokens: (Address, Address),
    decimals: (u8, u8),
    prices: &BTreeMap<Address, f64>,
) -> Option<f64> {
    let side = |a: U256, b: U256, token: Address, dec: u8| {
        let d = if a > b { a - b } else { b - a };
        prices.get(&token).map(|p| u256_f64(u256_to_ethers(d)) / 10f64.powi(i32::from(dec)) * p)
    };
    match (side(before.0, after.0, tokens.0, decimals.0), side(before.1, after.1, tokens.1, decimals.1)) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, y) => x.or(y),
    }
}
```

- In `usd_prices`, replace the `sqrt`, `raw` and `one_per_zero` lines with `let Some(one_per_zero) = pool_price(p) else { continue };`.
- `FeedHandler::held` becomes `Vec<(Held, UnixNanos)>`. `handle`'s log arm becomes:

```rust
            Notification::Log(l) => match l.topics.first() {
                Some(t) if *t == MINT || *t == BURN => vec![Effect::Reload(vec![l.address])],
                Some(t) if *t == SWAP || *t == PANCAKE_SWAP => match decode_swap(&l) {
                    Some(s) => self.hold(book, Held::Swap(s), now),
                    None => Vec::new(),
                },
                Some(t) if *t == SYNC => match decode_sync(&l) {
                    Some(s) => self.hold(book, Held::Sync(s), now),
                    None => Vec::new(),
                },
                _ => Vec::new(),
            },
```

- `hold(&mut self, book: &PoolBook, h: Held, now: UnixNanos)`: replace `decode_swap` with `h`, and use `h.pending()` and `h.block()`. In `flush` and `apply`, read pools and positions through `Held`'s accessors. `apply` maps `held` with `Held::write` into `book.apply_writes(&…)`.
- In `flush`, before `apply`, take `let before = book.snapshot();`. After `apply`, compute each applied item's notional in arrival order, tracking each pool's reserves through the burst:

```rust
        let prices = usd_prices(book);
        let mut reserves: BTreeMap<Address, (U256, U256)> = BTreeMap::new();
        let mut sized: Vec<Option<f64>> = Vec::with_capacity(held.len());
        for ((h, _), r) in held.iter().zip(&results) {
            let applied = matches!(r, SwapApplied::Updated | SwapApplied::NeedsReload);
            let p = before.get(&h.pool());
            sized.push(match (h, p) {
                (Held::Swap(s), Some(p)) => notional_usd(s, (p.spec.token0, p.spec.token1), p.decimals, &prices),
                (Held::Sync(s), Some(p)) => {
                    let prev = reserves.get(&s.pool).copied().or_else(|| {
                        p.reserves.map(|r| (u256_to_alloy(r.reserve0), u256_to_alloy(r.reserve1)))
                    });
                    let now = (s.reserve0, s.reserve1);
                    if applied {
                        reserves.insert(s.pool, now);
                    }
                    prev.and_then(|b| reserve_change_usd(b, now, (p.spec.token0, p.spec.token1), p.decimals, &prices))
                }
                _ => None,
            });
        }
```

  `moved` becomes `Vec<(&Held, Option<f64>)>`, built from `held`, `results` and `sized`. `event(&self, book, moved, observed_at)` folds `notional` over the carried sizes instead of calling `notional_usd`, and builds the delta from `h.tx()`, `h.log_index()` and `h.pool()`. The rest is unchanged. Import `u256_to_alloy` and `u256_to_ethers` from `apex_types::compat`, and `StateWrite`, `SyncWrite` and `PoolSnapshot` from `book`.
- Add a row to the module-doc table: "| `Sync` (Aerodrome v2) | held with the burst; its reserves replace the pool's in the burst's one write | in the burst's event, sized by its reserve change |".

`shadow/mod.rs`:
- The feed filter becomes `topics0: vec![SWAP, PANCAKE_SWAP, MINT, BURN, SYNC]`, with `SYNC` imported.
- Stats gain `fee_failures: AtomicU64`.
- In `head_loop`, add `let mut fees = Health::default();` and, after the TWAP line:

```rust
            fees.note("Aerodrome v2's fees", self.book.refresh_cp_fees(&self.reads, h.number).await.map(|_| ()), &self.stats.fee_failures);
```

- `ReadFailures` gains `pub fees: u64,`, filled `fees: get(&self.stats.fee_failures)`.

`scripts/shadow-status.sh` line 81: `print(f"  reads       view {rf['view']}, twap {rf['twap']}, l1 {rf['l1']}, fees {rf.get('fees', 0)} failures")`.

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 -p apex-runtime --test live_feed`, then `cargo test -j 3 -p apex-runtime`

---

### Task 6: The inventory, and checks against the chain

**Files:**
- Create: `scripts/data/build_aerodrome_v2_pools.py`, `scripts/data/verify_aerodrome_v2.py`
- Data (outside git): `data/base/aerodrome_v2/pools.jsonl`

**Interfaces:**
- Consumes: `arb_census.rpc.Rpc` (keyed BlockPI, a User-Agent, rate-limited, `batch`).
- Produces: the inventory file in the record format `inventory::load` reads, plus `stable`.

- [ ] **Step 1: Write `scripts/data/build_aerodrome_v2_pools.py`:**

```python
#!/usr/bin/env python3
"""Aerodrome v2's pools that pair with WETH, for the shadow's universe (R24).

Enumerates the factory (allPoolsLength, allPools(i)), keeps pools with WETH on
one side, and writes each with its tokens, stable flag, fee and depth to
data/base/aerodrome_v2/pools.jsonl, in the record format the live inventory
reads. Depth is the WETH reserve valued in USD, hub_symbol WETH, the way the
hub-side figure is measured elsewhere; the WETH price is the volatile
WETH/USDC pool's own. The shadow's filter then decides what loads.
"""
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from arb_census.rpc import Rpc  # noqa: E402

FACTORY = "0x420dd381b31aef6683db6b902084cb0ffece40da"
WETH = "0x4200000000000000000000000000000000000006"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
WETH_USDC = "0xcdac0d6c6c59727a65f871236188350531885c43"
OUT = Path(__file__).resolve().parents[2] / "data" / "base" / "aerodrome_v2" / "pools.jsonl"


def word(x):
    return hex(x)[2:].rjust(64, "0") if isinstance(x, int) else x[2:].lower().rjust(64, "0")


def main():
    rpc = Rpc(max_per_second=8)
    block = hex(int(rpc.call("eth_blockNumber", []), 16))
    call = lambda to, data: ("eth_call", [{"to": to, "data": data}, block])
    n = int(rpc.call(*call(FACTORY, "0xefde4e64")), 16)
    pools = ["0x" + r[-40:] for r in rpc.batch([call(FACTORY, "0x41d1de97" + word(i)) for i in range(n)]) if r]
    print(f"{n} pools at block {int(block, 16)}", flush=True)
    toks = rpc.batch([c for p in pools for c in (call(p, "0x0dfe1681"), call(p, "0xd21220a7"))])
    weth_pools = [(p, "0x" + toks[2 * i][-40:], "0x" + toks[2 * i + 1][-40:]) for i, p in enumerate(pools)
                  if toks[2 * i] and toks[2 * i + 1] and WETH in ("0x" + toks[2 * i][-40:], "0x" + toks[2 * i + 1][-40:])]
    print(f"{len(weth_pools)} with WETH", flush=True)
    detail = rpc.batch([c for p, _, _ in weth_pools for c in (
        call(p, "0x22be3de1"), call(p, "0x0902f1ac"))])
    r = rpc.call(*call(WETH_USDC, "0x0902f1ac"))
    weth_usd = int(r[66:130], 16) / 1e6 / (int(r[2:66], 16) / 1e18)
    rows = []
    for i, (p, t0, t1) in enumerate(weth_pools):
        stable, res = detail[2 * i], detail[2 * i + 1]
        if not stable or not res:
            continue
        is_stable = int(stable, 16) == 1
        fee = rpc.call(*call(FACTORY, "0xcc56b2c5" + word(p) + word(1 if is_stable else 0)))
        r0, r1 = int(res[2:66], 16), int(res[66:130], 16)
        weth_reserve = r0 if t0 == WETH else r1
        rows.append({"pool": p, "token0": t0, "token1": t1, "stable": is_stable, "fee": int(fee, 16),
                     "fee_ppm_onchain": int(fee, 16) * 100, "created_block": None,
                     "hub_usd_liquidity": round(weth_reserve / 1e18 * weth_usd, 2), "hub_symbol": "WETH"})
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text("\n".join(json.dumps(x) for x in sorted(rows, key=lambda x: -x["hub_usd_liquidity"])) + "\n")
    deep = [x for x in rows if not x["stable"] and x["fee_ppm_onchain"] <= 3000 and x["hub_usd_liquidity"] >= 100_000]
    print(f"wrote {len(rows)} to {OUT}; {len(deep)} volatile, <= 3,000 ppm, >= $100k (WETH at ${weth_usd:,.0f})")


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Run it.** `cd scripts/data && python3 build_aerodrome_v2_pools.py`. Expected: the WETH/USDC pool `0xcdac…` is among the deep volatile ones. Record the counts.

- [ ] **Step 3: Write `scripts/data/verify_aerodrome_v2.py`.** It takes the inventory records that pass the filter (volatile, ≤ 3,000 ppm, ≥ $100k) and checks, at one pinned block:

  1. **Inventory truth:** `factory()` is the factory, `isPool(pool)` is true, `stable()` is false, and `token0()`/`token1()` match the record.
  2. **Fee parity:** `getFee(pool,false) × 100` equals the record's `fee_ppm_onchain`.
  3. **Quote parity:** the R24 formula over `getReserves()` equals `getAmountOut(amountIn, tokenIn)` at 0.01, 0.1 and 1 WETH, and at the USD equivalents in the pool's other token, both directions. To the unit.
  4. **Sync parity:** over the last 200 blocks, each pool's last `Sync` in a block equals `getReserves()` at that block.

  It prints one line per check and exits non-zero on any failure:

```python
#!/usr/bin/env python3
"""Check Aerodrome v2's filtered inventory against the chain (R24 spec checks 1, 2, 3, 5)."""
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from arb_census.rpc import Rpc  # noqa: E402

FACTORY = "0x420dd381b31aef6683db6b902084cb0ffece40da"
WETH = "0x4200000000000000000000000000000000000006"
SYNC = "0xcf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a"
INV = Path(__file__).resolve().parents[2] / "data" / "base" / "aerodrome_v2" / "pools.jsonl"


def w(x):
    return hex(x)[2:].rjust(64, "0") if isinstance(x, int) else x[2:].lower().rjust(64, "0")


def quote(a, fee_ppm, r_in, r_out):
    a = a - a * fee_ppm // 1_000_000
    return a * r_out // (r_in + a)


def main():
    rpc = Rpc(max_per_second=8)
    head = int(rpc.call("eth_blockNumber", []), 16)
    b = hex(head - 2)
    c = lambda to, data, blk=b: rpc.call("eth_call", [{"to": to, "data": data}, blk])
    recs = [json.loads(l) for l in INV.read_text().splitlines() if l.strip()]
    recs = [r for r in recs if not r["stable"] and r["fee_ppm_onchain"] <= 3000 and r["hub_usd_liquidity"] >= 100_000]
    bad = 0
    for r in recs:
        p = r["pool"]
        truth = ("0x" + c(p, "0xc45a0155")[-40:] == FACTORY and int(c(FACTORY, "0x5b16ebb7" + w(p)), 16) == 1
                 and int(c(p, "0x22be3de1"), 16) == 0 and "0x" + c(p, "0x0dfe1681")[-40:] == r["token0"]
                 and "0x" + c(p, "0xd21220a7")[-40:] == r["token1"])
        fee = int(c(FACTORY, "0xcc56b2c5" + w(p) + w(0)), 16) * 100
        res = c(p, "0x0902f1ac")
        r0, r1 = int(res[2:66], 16), int(res[66:130], 16)
        weth_is_0 = r["token0"] == WETH
        other_per_weth = (r1 / r0) if weth_is_0 else (r0 / r1)
        mismatches = 0
        for weth_amt in (10**16, 10**17, 10**18):
            for z, amt in ((weth_is_0, weth_amt), (not weth_is_0, max(1, int(weth_amt * other_per_weth)))):
                tok = r["token0"] if z else r["token1"]
                chain = int(c(p, "0xf140a35a" + w(amt) + w(tok)), 16)
                mine = quote(amt, fee, r0, r1) if z else quote(amt, fee, r1, r0)
                mismatches += chain != mine
        ok = truth and fee == r["fee_ppm_onchain"] and mismatches == 0
        bad += not ok
        print(f"{p} truth {truth} fee {fee} quotes off {mismatches}/6 {'OK' if ok else 'FAIL'}")
    logs = rpc.call("eth_getLogs", [{"address": [r["pool"] for r in recs], "topics": [SYNC],
                                     "fromBlock": hex(head - 200), "toBlock": hex(head - 2)}])
    last = {}
    for l in logs:
        last[(l["address"].lower(), int(l["blockNumber"], 16))] = l["data"]
    sync_bad = 0
    for (p, blk), data in last.items():
        res = c(p, "0x0902f1ac", hex(blk))
        sync_bad += res[2:130] != data[2:130]
    print(f"Sync parity: {len(last) - sync_bad}/{len(last)} pool-blocks equal getReserves")
    bad += sync_bad
    print(f"{len(recs)} pools checked at block {int(b, 16)}: {'all OK' if bad == 0 else f'{bad} FAILED'}")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
```

- [ ] **Step 4: Run it:** `cd scripts/data && python3 verify_aerodrome_v2.py`. Expected: every pool OK and Sync parity n/n. Any failure is a defect to fix before going on.

---

### Task 7: Gates, mutation checks, the owner transaction, commit

- [ ] **Step 1: Gates.** `bash $SCRATCH/gates.sh` must pass, apart from the two known-red scripts (`check_placeholder_endpoints.sh`, `cl_parity_sweep.sh`).

- [ ] **Step 2: Mutation checks.** Mutate each item below, run the named tests with `--no-fail-fast`, `touch` the file after restoring it, and confirm every mutant is killed. Delete any guard that survives.
  - `cp::quote_out`'s fee: `amount_in - fee` → `amount_in * (1e6 - fee_ppm) / 1e6`. Run `live_cp`.
  - The `stable` checks:
    - `decode_state`'s `!= 0` → `== 2`. Run `live_book`.
    - The inventory's `r.stable != Some(false)` → `r.stable == Some(true)`. Run `live_venues`.
  - The `isPool` check: `|| known != 1` removed. Run `live_book`.
  - The kind guards in `sync_into` and `swap_into` (`.filter(…)` removed). Run `live_book`.
  - The reload keep: `snap.reserves = old.reserves;` removed. Run `live_book`.
  - The pricing dispatch: the `Some(r)` arm forced to the ladder path. Run `live_pricing`.
  - `max_input`: the reserve sides swapped. Run `live_pricing`.
  - `reserve_change_usd`: `x.max(y)` → `x.min(y)`. Run `live_feed`.

- [ ] **Step 3: Prepare and dry-run the owner transaction:**

```bash
R=0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2; X=0x8940B565D050974b2b589B70De43bCb753b1F93B
REG=$(cast calldata "registerAdapter(uint16,address)" 4 0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43)
ALLOW=$(cast calldata "allowSelector(uint16,bytes4)" 4 0xcac88ea9)
CALL=$(cast calldata "multicall(address[],bytes[])" "[$X,$X]" "[$REG,$ALLOW]")
echo "$CALL" > $SCRATCH/r24_register_calldata.txt
K=$(grep ^BLOCKPI_KEY= .env | cut -d= -f2-)
BLOCKPI_KEY=$K cast call --from 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3 $R "$CALL" --rpc-url base
BLOCKPI_KEY=$K cast estimate --from 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3 $R "$CALL" --rpc-url base
BLOCKPI_KEY=$K cast call --from 0xCB436Ba3acb945b3fc8EE6345857262584356595 $R "$CALL" --rpc-url base   # expect NotOwner
```

Expected: the owner's call succeeds, and the trader's reverts `NotOwner`. Give the user the calldata, the target `R`, and the gas.

- [ ] **Step 4: PLAN.md R24 entry**, after R23. It records:
  - the evidence;
  - what changed;
  - the verification results: pools and counts, fee and quote parity, Sync parity, and the measured gas;
  - the mutants;
  - that the venue stays out until adapter 4 is registered.

- [ ] **Step 5: Commit by named paths:**
  - `crates/apex-exec/src/encode/mod.rs`, `crates/apex-exec/tests/generic_step.rs`.
  - The `crates/apex-runtime/src/live/` files: `cp.rs`, `mod.rs`, `abi.rs`, `book.rs`, `pricing.rs`, `frontier.rs`, `feed.rs`, `calls.rs`, `gas.rs`, `admission.rs`, `sim.rs`, `inventory.rs`.
  - `crates/apex-runtime/src/shadow/mod.rs`.
  - The `crates/apex-runtime/tests/` files that changed.
  - `scripts/shadow-status.sh`, `scripts/data/build_aerodrome_v2_pools.py`, `scripts/data/verify_aerodrome_v2.py`, `PLAN.md`.

  Then check that `git diff --stat -- <those paths>` is empty. Verify the commit in a clean worktree with its own `CARGO_TARGET_DIR`: `cargo test -j 3 -p apex-runtime -p apex-exec`.

---

### Task 8: After the operator broadcasts the registration

- [ ] **Step 1:** Confirm on chain: `adapterOf(4)` is the router, and `isSelectorAllowed(4, 0xcac88ea9)` is true.
- [ ] **Step 2: Execution and gas (spec check 4).** Run `eth_simulateV1` of the executor's `startV2` over a two-hop cycle: one Aerodrome v2 hop (WETH/USDC `0xcdac…`) and one Uniswap v3 hop. Use the R12 method: the trader as sender, a whale override moving one pool first. It must settle and return the book's predicted gross to the wei, with gas inside the model's ceiling. If its expected gas differs from `hop.aerodrome_v2` by more than 10,000, set the measured figure, then re-run the gates and commit.
- [ ] **Step 3: Release build and restart.**
  - `cargo build -j 3 --release`. Stop the shadow by PID, archive its logs as `postdeploy-16`, start it, and confirm it boots with `AerodromeV2` reachable.
  - Record the pool and route counts (spec check 6) in PLAN.md's R24 entry, and commit.
  - The restart resets the 14-day clock; tell the operator from when.
- [ ] **Step 4:** Update memory: `shadow-run-operations` for the new run, and `base-mev-market-size` with what R24 added.
