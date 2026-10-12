# Armed backruns, armed-shadow and evaluator (Task 8.5 R26) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Arm every top route's gap as a pre-signed conditional transaction (armed-shadow: recorded, never sent), and judge each arm against the chain: would it have fired, won and earned, at which bid.

**Architecture:** `apex-math::cl_move` gives a pool's exact state at any price. `apex-runtime::armed` finds each slot's threshold rungs, encodes the predicates and break-even calldata, and runs as a shadow worker writing an armed ledger. `apex armed-eval` (a systemd timer on the host) rebuilds every watched storage word from logs, finds fire points and rivals, quotes the fire exactly and checks it with `eth_simulateV1`, and writes verdicts and a bid curve.

**Tech Stack:** Rust (apex-math, apex-chain, apex-runtime), serde/serde_json, tokio, ethers-core U256 in the math, alloy primitives in the runtime, systemd.

**Spec:** `docs/superpowers/specs/2026-10-13-armed-backruns-design.md` (8fc0e8d).

## Scope

This plan is the spec's rollout steps 1–3: armed-shadow and the evaluator. The live pilot (spec §4: senders, submission, caps, accounting, tripwires, the API probe) is a second plan, written at gate A from the shadow's bid curve. `armed.mode: live` is refused at boot until then.

## Corrections to the spec, from reading the executor

1. **Hop 1's floor is `mid`, not 1.** The plan fixes hop 2's input at `mid`, the hop-1 output at the rung's threshold. If hop 1 returns less, hop 2 cannot spend `mid` and the transaction reverts whatever hop 1's floor says, so a floor of 1 protects nothing. The floors are: hop 1 `minOut = mid`; hop 2 `minOut = loan + 1`; `minProfit = 1`.
2. **Overshoot lands as residue, not WETH.** When the trigger moves past the threshold, hop 1 returns more than `mid`. Hop 2 still sells exactly `mid`, and the excess intermediate token stays in the executor. `_assertSettled` checks only the loan token, so this does not revert. The evaluator reports WETH gross and that residue separately.
3. **No `verify_pool_slots.py`.** The engine's boot check (`verify_layouts`, Task 5) reads every universe pool's watched words at the boot block and compares them with the freshly loaded book. A pool that fails is left unarmable. A venue table test pins the layout. An offline script would only repeat that check.

## Changes found by the smoke runs (2026-10-12 UTC, executing this plan)

Two local release smoke runs against mainnet, each with the evaluator, changed the following. Each change has its own test.

1. **Rungs are additive: the threshold, then +5 and +20 ticks (or bps), not ×2 and ×4.**
   - The pools sit a few ticks apart, so the threshold m₁ is often 1–4, and 2m₁ and 4m₁ collapse onto it.
   - When m₁ is large, 4m₁ is beyond any move seen.
   - Code: `RUNG_OFFSETS = [0, 5, 20]`.
2. **For a hop-2 trigger, the band's `mid` is the least hop 1 returns across the band.** Without this, leg-1 slots never armed: hop 1's adverse edge always fell short of the rung's `mid`, so every adverse edge would have reverted.
3. **The ranking's prior counts only for moves no larger than the largest seen.** Otherwise the Laplace prior gives an unseen 2,000-tick move the same odds as a seen 3-tick one.
4. **The live move check counts wrong ticks and liquidity changes separately.**
   - Wrong ticks are the model's error, and were 0 in 6,692 real swaps.
   - Liquidity changes are mints and burns the book had not applied, 7–13% of swaps on Base.
   - `move_checks()` is `(checked, tick_mismatched, liquidity_changed)`, and the gate is on wrong ticks.
5. **`eth_simulateV1` call objects carry `data`, not `input`**, as `live/sim.rs` does.
6. **Only rungs priced at the pools' current versions are ranked, armed or renewed.**
   - The engine reprices 24 of 308 slots a pass (about 16 ms each). The first smoke run armed from rungs priced up to ~5 s earlier, while `start_words` came from the current book.
   - Its bands sat 2–4 ticks off the other pool's price, so 336 arms fired 0.
   - `reprice_budget` is now an `EngineSettings` field (24 in the shadow), so a test can starve it.
7. **Verdicts record `trigger_met`, and the summary splits unfired arms** into `unfired_trigger_unmet` and `unfired_band_missed`. Those two call for opposite tuning: rung steps, or band widths and arm age.
8. **An arm's window ends when its last block is the head.**
   - The engine's head comes from `newHeads`, a sealed block. So at `head == to_block` the window is over: the sender is freed and an unchanged arm renewed then, valid from the next block.
   - The plan's `busy < head` and `to_block < head` were one block late. Live, that left one block in six with no valid transaction. The shadow's merged windows would have counted fires in that block, overstating coverage.
   - Found by the sender-reuse mutant surviving.

## Global Constraints

- Never print, echo or log a secret (`APEX_SECRET_*`, keys); the armed ledger holds calldata and predicates only.
- Never `cargo fmt`. Use `-j 3` for every cargo command (15 GB machine).
- Stage by named paths only, never `git add -A`; after committing, `git diff --stat -- <paths>` must be empty.
- Before committing, run all five gates (`$SCRATCH/gates.sh`). The two known-red scripts are `check_placeholder_endpoints.sh` and `cl_parity_sweep.sh`.
- Verify the commit in a clean worktree with its own `CARGO_TARGET_DIR`.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Venue storage layout (verified on chain 2026-10-12): price word (mask 2^160−1) at slot 0 for Uniswap v3 and PancakeSwap v3, slot 6 for Slipstream and SlipstreamV3; liquidity (mask 2^128−1) at slot 4 (Uniswap v3), 5 (PancakeSwap v3), 16 (Slipstream, SlipstreamV3); Aerodrome v2 reserves at slots 20 and 21.
- Defaults: `max_arms: 16`, `rungs: 3` (m₁, m₁ + 5, m₁ + 20; see "Changes found by the smoke runs"), `window_s: 10`, `block_time_ms: 2000`, `margin_wei: 0`, `band_cap_bps: 5`, `liquidity_floor_bps: 9800`; bid fractions `f` ∈ {0.05, 0.1, 0.2, 0.3, 0.5, 0.7, 0.9}.
- No `flashblock_index` predicate anywhere (Denim).

## File structure

| File | Responsibility |
|---|---|
| `crates/apex-math/src/cl_move.rs` (new) | `move_to_price`: exact state at a target price |
| `crates/apex-runtime/src/live/cp.rs` | `move_reserves` |
| `crates/apex-runtime/src/live/book.rs` | `Move`, `PoolSnapshot::moved`, live move checks |
| `crates/apex-runtime/src/live/pricing.rs` | `LiveCycle::quote_fixed`, `CostedCycle::cycle` |
| `crates/apex-runtime/src/live/calls.rs` | `encode_step` (free function, shared) |
| `crates/apex-runtime/src/live/reads.rs` | `ChainReads::storage_at` |
| `crates/apex-runtime/src/armed/mod.rs` (new) | module root |
| `crates/apex-runtime/src/armed/threshold.rs` (new) | slots, threshold search, rungs, band |
| `crates/apex-runtime/src/armed/predicate.rs` (new) | layouts, predicate JSON |
| `crates/apex-runtime/src/armed/plan.rs` (new) | break-even executor call |
| `crates/apex-runtime/src/armed/lines.rs` (new) | armed-ledger line types |
| `crates/apex-runtime/src/armed/moves.rs` (new) | move statistics for ranking |
| `crates/apex-runtime/src/armed/engine.rs` (new) | ranking, senders, arm/renew lines, layout check |
| `crates/apex-runtime/src/armed/eval.rs` (new) | pure judging: words, fire, rival, landing, net |
| `crates/apex-runtime/src/armed/run.rs` (new) | the evaluator's I/O, verdicts, summary |
| `crates/apex-runtime/src/shadow/{mod,config}.rs` | armed worker, config, report |
| `crates/apex-runtime/src/main.rs` | `apex armed-eval` |
| `crates/apex-chain/src/rpc/ws.rs` | `parse_log` |
| `crates/apex-chain/src/base/flashblock_stream.rs` | `FlashblockSeen: Deserialize` |
| `ops/shadow.base.yaml`, `ops/host/*`, `scripts/shadow-status.sh` | config, timer, runbook, status |

---

### Task 1: `move_to_price` (apex-math)

**Files:**
- Create: `crates/apex-math/src/cl_move.rs`
- Modify: `crates/apex-math/src/lib.rs` (add `pub mod cl_move;` after `pub mod cl_math;`)
- Test: `crates/apex-math/tests/cl_move.rs`

**Interfaces:**
- Produces: `apex_math::cl_move::move_to_price(state: &ClPoolState, ladder: &TickLadder, target: U256) -> Option<ClPoolState>`.

- [ ] **Step 1: Write the failing test** `crates/apex-math/tests/cl_move.rs`:

```rust
//! R26: a pool moved to a price is exactly the pool a swap to that price leaves.

use apex_math::cl_math::{get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio};
use apex_math::cl_move::move_to_price;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::{quote_exact_input_multi_tick, TickLadder};
use ethers_core::types::U256;

fn ladder() -> TickLadder {
    TickLadder::new(
        vec![
            (-180, 1_000_000_000),
            (-120, 2_000_000_000),
            (-60, 3_000_000_000),
            (60, -3_000_000_000),
            (120, -2_000_000_000),
            (180, -1_000_000_000),
        ],
        -180,
        180,
    )
}

fn state() -> ClPoolState {
    ClPoolState {
        sqrt_price_x96: get_sqrt_ratio_at_tick(0).unwrap(),
        liquidity: 6_000_000_000,
        tick: 0,
        tick_spacing: 60,
        fee_ppm: 3_000,
        balance0: Some(U256::from(10u128.pow(30))),
        balance1: Some(U256::from(10u128.pow(30))),
    }
}

/// The step-exact quoter (checked to the wei against Uniswap's and Slipstream's
/// quoters) leaves a price, a liquidity and a tick; moving there must agree.
#[test]
fn a_move_lands_where_a_swap_to_that_price_does() {
    let (s, l) = (state(), ladder());
    let mut crossed = 0;
    for zero_for_one in [true, false] {
        for amount in [10u128.pow(6), 10u128.pow(8), 2 * 10u128.pow(8), 5 * 10u128.pow(8), 10u128.pow(9)] {
            let q = quote_exact_input_multi_tick(&s, &l, U256::from(amount), zero_for_one, 128).unwrap();
            if q.exhausted || q.ended_on_tick_boundary {
                continue;
            }
            crossed += usize::from(q.ticks_crossed > 0);
            let m = move_to_price(&s, &l, q.sqrt_price_after).expect("inside the ladder");
            assert_eq!(m.sqrt_price_x96, q.sqrt_price_after);
            assert_eq!(m.liquidity, q.liquidity_after, "amount {amount} zero_for_one {zero_for_one}");
            assert_eq!(m.tick, get_tick_at_sqrt_ratio(q.sqrt_price_after).unwrap());
            // What left the pool: at least the quote's output (it rounds up), at most a unit per step more.
            let (before, after) = if zero_for_one { (s.balance1, m.balance1) } else { (s.balance0, m.balance0) };
            let out = before.unwrap() - after.unwrap();
            assert!(out >= q.amount_out && out <= q.amount_out + U256::from(8u8), "out {out} quote {}", q.amount_out);
        }
    }
    assert!(crossed >= 2, "the vectors must cross ticks");
}

/// v3-core crosses a tick eagerly on reaching its price: downward it leaves
/// the tick one below.
#[test]
fn a_target_on_an_initialized_tick_crosses_it() {
    let (s, l) = (state(), ladder());
    let down = move_to_price(&s, &l, get_sqrt_ratio_at_tick(-60).unwrap()).unwrap();
    assert_eq!((down.tick, down.liquidity), (-61, 3_000_000_000));
    let up = move_to_price(&s, &l, get_sqrt_ratio_at_tick(60).unwrap()).unwrap();
    assert_eq!((up.tick, up.liquidity), (60, 3_000_000_000));
}

/// Past what the ladder proves, nothing is assumed.
#[test]
fn a_target_past_the_ladder_is_refused() {
    let (s, l) = (state(), ladder());
    assert!(move_to_price(&s, &l, get_sqrt_ratio_at_tick(-170).unwrap()).is_some());
    assert!(move_to_price(&s, &l, get_sqrt_ratio_at_tick(-200).unwrap()).is_none());
    assert!(move_to_price(&s, &l, get_sqrt_ratio_at_tick(250).unwrap()).is_none());
    let unchanged = move_to_price(&s, &l, s.sqrt_price_x96).unwrap();
    assert_eq!((unchanged.tick, unchanged.liquidity), (0, 6_000_000_000));
}
```

- [ ] **Step 2: Run it, expect a compile failure** (no `cl_move`): `cargo test -j 3 --offline -p apex-math --test cl_move`

- [ ] **Step 3: Implement** `crates/apex-math/src/cl_move.rs`:

```rust
//! Where a concentrated-liquidity pool's state lands when its price moves to a
//! target (Task 8.5 R26).
//!
//! The state at a price does not depend on the swaps that took it there, only
//! on the liquidity it crossed: walking the ladder to the target and crossing
//! each initialized tick gives exactly what the pool will hold. An armed
//! backrun is priced against that state before any trigger exists.
//!
//! Crossing follows v3-core: reaching an initialized tick's price crosses it,
//! and a downward crossing leaves the tick one below it. Balances move by what
//! flows between the prices, fee-less: the side leaving the pool rounds up and
//! the side entering it rounds down, so the pool is never shown richer.

use crate::cl_math::{
    get_amount0_delta, get_amount1_delta, get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, max_sqrt_ratio,
    min_sqrt_ratio,
};
use crate::cl_state::ClPoolState;
use crate::cl_swap::{LadderStep, TickLadder};
use ethers_core::types::U256;

/// Initialized ticks a move may cross: the ladder's own scale, with room.
const MAX_CROSSINGS: usize = 10_000;

fn apply_net(liquidity: u128, net: i128) -> Option<u128> {
    if net >= 0 {
        liquidity.checked_add(net.unsigned_abs())
    } else {
        liquidity.checked_sub(net.unsigned_abs())
    }
}

/// `state` with its price at `target`: tick, liquidity and balances as the pool
/// would hold them. `None` when the move would leave the ladder's proven range,
/// drain liquidity or a balance, or the target is outside the protocol's prices.
pub fn move_to_price(state: &ClPoolState, ladder: &TickLadder, target: U256) -> Option<ClPoolState> {
    if state.liquidity == 0 || target < min_sqrt_ratio() || target >= max_sqrt_ratio() || !ladder.covers(state.tick) {
        return None;
    }
    let mut s = state.clone();
    if target == s.sqrt_price_x96 {
        return Some(s);
    }
    let down = target < s.sqrt_price_x96;
    let (mut into, mut out) = (U256::zero(), U256::zero());
    for _ in 0..MAX_CROSSINGS {
        let next = match ladder.next_initialized(s.tick, down) {
            LadderStep::Initialized { tick, liquidity_net } => Some((tick, liquidity_net, get_sqrt_ratio_at_tick(tick)?)),
            LadderStep::Exhausted => None,
        };
        let crossing = next.filter(|(_, _, at)| if down { target <= *at } else { target >= *at });
        let stop = crossing.map_or(target, |(_, _, at)| at);
        // With no initialized tick before the target, the ladder must prove the
        // target itself, or the liquidity on the way is unknown.
        if crossing.is_none() && !ladder.covers(get_tick_at_sqrt_ratio(target)?) {
            return None;
        }
        let (from, to) = (s.sqrt_price_x96, stop);
        if down {
            into = into.checked_add(get_amount0_delta(from, to, s.liquidity, false)?)?;
            out = out.checked_add(get_amount1_delta(from, to, s.liquidity, true)?)?;
        } else {
            into = into.checked_add(get_amount1_delta(from, to, s.liquidity, false)?)?;
            out = out.checked_add(get_amount0_delta(from, to, s.liquidity, true)?)?;
        }
        s.sqrt_price_x96 = stop;
        let Some((tick, net, _)) = crossing else {
            s.tick = get_tick_at_sqrt_ratio(stop)?;
            return settle(s, down, into, out);
        };
        let signed = if down { net.checked_neg()? } else { net };
        s.liquidity = apply_net(s.liquidity, signed).filter(|l| *l > 0)?;
        s.tick = if down { tick - 1 } else { tick };
        if stop == target {
            return settle(s, down, into, out);
        }
    }
    None
}

fn settle(mut s: ClPoolState, down: bool, into: U256, out: U256) -> Option<ClPoolState> {
    let (entering, leaving) = if down { (&mut s.balance0, &mut s.balance1) } else { (&mut s.balance1, &mut s.balance0) };
    if let Some(b) = entering.as_mut() {
        *b = b.checked_add(into)?;
    }
    if let Some(b) = leaving.as_mut() {
        *b = b.checked_sub(out)?;
    }
    Some(s)
}
```

Add `pub mod cl_move;` to `crates/apex-math/src/lib.rs` after `pub mod cl_math;`.

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 --offline -p apex-math --test cl_move`. If `a_move_lands_where_a_swap_to_that_price_does` reports `crossed < 2`, raise the largest amounts (the ladder's L is 6e9: about 3e8 reaches tick ±60) until two vectors cross, and say so in the test's comment.

---

### Task 2: Moved snapshots and the live move check

**Files:**
- Modify: `crates/apex-runtime/src/live/cp.rs` (add `move_reserves`)
- Modify: `crates/apex-runtime/src/live/book.rs` (add `Move`, `PoolSnapshot::moved`, `MoveChecks`, the check in `apply_writes`, `PoolBook::move_checks`)
- Test: `crates/apex-runtime/tests/armed_move.rs`

**Interfaces:**
- Consumes: Task 1's `move_to_price`.
- Produces:
  - `live::cp::move_reserves(r: &Reserves, up: bool, bps: u32) -> Option<Reserves>`;
  - `live::book::Move::{Tick(i32), Bps { up: bool, bps: u32 }}`;
  - `PoolSnapshot::moved(&self, to: Move) -> Option<PoolSnapshot>`;
  - `PoolBook::move_checks(&self) -> (u64, u64)` (checked, mismatched).

- [ ] **Step 1: Write the failing tests** `crates/apex-runtime/tests/armed_move.rs`:

```rust
//! R26: pools moved to a price, for pricing an armed backrun before its trigger.

use alloy_primitives::{address, Address, U256 as AU256};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::live::book::{Move, PoolBook, PoolSnapshot, SwapWrite};
use apex_runtime::live::cp::{move_reserves, Reserves};
use apex_runtime::live::frontier::WETH;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_types::state::ReconstructionStatus;
use ethers_core::types::U256;

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const L: u128 = 1_400_000_000_000_000_000;

fn pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32) -> PoolSnapshot {
    let spacing = 10;
    let lower = (tick - 2_000) / spacing * spacing;
    let upper = (tick + 2_000) / spacing * spacing;
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 5_000_000.0 },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: L,
            tick,
            tick_spacing: spacing,
            fee_ppm,
            balance0: Some(U256::from(10u128.pow(21))),
            balance1: Some(U256::from(3_000_000_000_000u128)),
        },
        ladder: TickLadder::new(vec![(lower, L as i128), (upper, -(L as i128))], lower - 5_000, upper + 5_000),
        decimals: (18, 6),
        factory: venue.factory(),
        code_hash: Default::default(),
        block: 100,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
        reserves: None,
    }
}

#[test]
fn reserves_move_along_the_curve_and_never_gain() {
    let r = Reserves { reserve0: U256::from(10u128.pow(21)), reserve1: U256::from(3 * 10u128.pow(12)) };
    let k = r.reserve0 * r.reserve1;
    for (up, bps) in [(true, 100u32), (false, 100), (true, 1), (false, 2_000)] {
        let m = move_reserves(&r, up, bps).unwrap();
        let before = r.reserve1.as_u128() as f64 / r.reserve0.as_u128() as f64;
        let after = m.reserve1.as_u128() as f64 / m.reserve0.as_u128() as f64;
        let want = if up { 1.0 + f64::from(bps) / 1e4 } else { 1.0 - f64::from(bps) / 1e4 };
        assert!((after / before - want).abs() < 1e-6, "{up} {bps}: {}", after / before);
        assert!(m.reserve0 * m.reserve1 <= k, "the product never grows");
    }
    assert!(move_reserves(&r, false, 10_000).is_none());
    assert!(move_reserves(&Reserves { reserve0: U256::zero(), reserve1: U256::one() }, true, 1).is_none());
}

#[test]
fn a_snapshot_moves_by_its_kind() {
    let p = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let up = p.moved(Move::Tick(-197_300)).unwrap();
    assert_eq!(up.state.tick, -197_300);
    assert_eq!(up.state.sqrt_price_x96, get_sqrt_ratio_at_tick(-197_300).unwrap());
    assert!(p.moved(Move::Bps { up: true, bps: 10 }).is_none(), "a tick pool takes ticks");
    assert!(p.moved(Move::Tick(-100_000)).is_none(), "outside the ladder");
}

/// Every Swap the book applies is checked against `move_to_price` from the
/// state before it: the move model is validated on every real swap.
#[test]
fn applied_swaps_are_checked_against_the_move_model() {
    let book = PoolBook::from_snapshots(vec![pool(UNI, Venue::UniswapV3, -197_350, 500)], ReconstructionStatus::Verified);
    let target = get_sqrt_ratio_at_tick(-197_340).unwrap();
    let mut target_alloy = [0u8; 32];
    target.to_big_endian(&mut target_alloy);
    let sqrt = AU256::from_be_bytes(target_alloy);
    book.apply_swaps(&[SwapWrite { pool: UNI, sqrt_price_x96: sqrt, liquidity: L, tick: -197_340, at: (101, 1) }]);
    assert_eq!(book.move_checks(), (1, 0));
    book.apply_swaps(&[SwapWrite { pool: UNI, sqrt_price_x96: sqrt, liquidity: L + 1, tick: -197_340, at: (102, 1) }]);
    assert_eq!(book.move_checks(), (2, 1), "a liquidity the model does not predict is a mismatch");
}
```

- [ ] **Step 2: Run, expect compile failures:** `cargo test -j 3 --offline -p apex-runtime --test armed_move`

- [ ] **Step 3: Implement.**

`crates/apex-runtime/src/live/cp.rs`, after `quote_out`:

```rust
/// Reserves after token0's price in token1 (`reserve1 / reserve0`) moves by
/// `bps` basis points, up or down, along `x·y = k` (R26). The product never
/// grows, so a moved pool is never shown richer than the curve allows. `None`
/// for an empty side, a move of 10,000 bps or more down, or overflow.
pub fn move_reserves(r: &Reserves, up: bool, bps: u32) -> Option<Reserves> {
    if r.reserve0.is_zero() || r.reserve1.is_zero() || bps == 0 || (!up && bps >= 10_000) {
        return None;
    }
    let k = r.reserve0.checked_mul(r.reserve1)?;
    let moved = U256::from(if up { 10_000 + bps } else { 10_000 - bps });
    // p' = p · moved / 10⁴, and reserve0' = sqrt(k / p') = reserve0 · sqrt(10⁴ / moved).
    let reserve0 = (r.reserve0.checked_mul(r.reserve0)?.checked_mul(U256::from(10_000u32))? / moved).integer_sqrt();
    if reserve0.is_zero() {
        return None;
    }
    Some(Reserves { reserve0, reserve1: k / reserve0 })
}
```

`crates/apex-runtime/src/live/book.rs`:
- after `pub type LogPosition`:

```rust
/// Where a pool's price would be moved, to price an armed backrun (R26).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    /// A concentrated pool's price, to this tick's sqrt price.
    Tick(i32),
    /// A constant-product pool's price of token0 in token1, by basis points.
    Bps { up: bool, bps: u32 },
}
```

- inside `impl PoolSnapshot`, after `ladder_covers_price`:

```rust
    /// This pool after its price moved `to`, crossed exactly and with the fee
    /// set for the new tick (R26). `None` past what the book proves or for a
    /// move of the other kind.
    pub fn moved(&self, to: Move) -> Option<Self> {
        let mut next = self.clone();
        match (to, &self.reserves) {
            (Move::Bps { up, bps }, Some(r)) => next.reserves = Some(crate::live::cp::move_reserves(r, up, bps)?),
            (Move::Tick(t), None) => {
                let target = apex_math::cl_math::get_sqrt_ratio_at_tick(t)?;
                next.state = apex_math::cl_move::move_to_price(&self.state, &self.ladder, target)?;
                next.fee_from_tick();
            }
            _ => return None,
        }
        Some(next)
    }
```

- a counter pair, and a `moves: MoveChecks` field on `PoolBook` initialised to `MoveChecks::default()` in `from_snapshots` (every other constructor spreads `..book`):

```rust
/// How the move model fared against every applied Swap (R26).
#[derive(Debug, Default)]
struct MoveChecks {
    checked: std::sync::atomic::AtomicU64,
    mismatched: std::sync::atomic::AtomicU64,
}
```

- in `apply_writes`, check each Swap before it is applied, against the pool as the writes before it left it:

```rust
    pub fn apply_writes(&self, writes: &[StateWrite]) -> Vec<SwapApplied> {
        use std::sync::atomic::Ordering::Relaxed;
        let (applied, checks) = self.write(|map, w, status| {
            let mut checks = (0u64, 0u64);
            let applied = writes
                .iter()
                .map(|s| match s {
                    StateWrite::Swap(s) => {
                        if let Some(old) = map.get(&s.pool).filter(|o| o.reserves.is_none() && Self::is_newer(o, s.at)) {
                            let target = u256_to_ethers(s.sqrt_price_x96);
                            if let Some(m) = apex_math::cl_move::move_to_price(&old.state, &old.ladder, target) {
                                checks.0 += 1;
                                checks.1 += u64::from(m.liquidity != s.liquidity || m.tick != s.tick);
                            }
                        }
                        Self::swap_into(map, w.seq, s)
                    }
                    StateWrite::Sync(s) => Self::sync_into(map, w.seq, s),
                })
                .collect();
            ((applied, checks), status)
        });
        self.moves.checked.fetch_add(checks.0, Relaxed);
        self.moves.mismatched.fetch_add(checks.1, Relaxed);
        applied
    }

    /// Swaps checked against the move model, and how many it got wrong (R26).
    /// A mismatch is the model's error or a liquidity change the book had not
    /// applied yet (a Mint or Burn waiting on its reload).
    pub fn move_checks(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.moves.checked.load(Relaxed), self.moves.mismatched.load(Relaxed))
    }
```

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 --offline -p apex-runtime --test armed_move --test live_book --test live_cp`

---

### Task 3: Thresholds, rungs and bands

**Files:**
- Create: `crates/apex-runtime/src/armed/mod.rs`, `crates/apex-runtime/src/armed/threshold.rs`
- Modify: `crates/apex-runtime/src/lib.rs` (`pub mod armed;`), `crates/apex-runtime/src/live/pricing.rs` (`LiveCycle::hop`, `LiveCycle::quote_fixed`, `CostedCycle::cycle`)
- Test: `crates/apex-runtime/tests/armed_threshold.rs`

**Interfaces:**
- Consumes: Task 2's `Move`, `PoolSnapshot::moved`.
- Produces:
  - `LiveCycle::quote_fixed(&self, amount_in: U256, mid: U256) -> Option<CycleQuote>`;
  - `CostedCycle::cycle(&self) -> &LiveCycle`;
  - `armed::threshold::{Pricing, Point, Rung, trigger_up, move_of, best_at, rungs, band, MAX_STEP}`:

```rust
pub struct Pricing { pub costs: RouteCosts, pub gas: SettlementGas, pub cap: Option<U256>, pub margin_wei: U256 }
pub struct Point { pub size: U256, pub mid: U256, pub output: U256, pub cost: U256, pub surplus: U256, pub gas: GasEstimate }
pub struct Rung { pub index: u8, pub step: u32, pub point: Point, pub trigger: PoolSnapshot }
pub fn rungs(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, px: &Pricing, count: u8) -> Vec<Rung>;
pub fn band(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, rung: &Rung, px: &Pricing, cap_steps: u32) -> Option<[PoolSnapshot; 2]>;
```

- [ ] **Step 1: Write the failing tests** `crates/apex-runtime/tests/armed_threshold.rs`. Copy the `pool` helper and constants from Task 2's test file (same body), and add a `SLIP` constant `address!("dbc6998296caA1652A810dc8D3BaF4A8294330f1")`:

```rust
use apex_math::finite_size::SearchBudget;
use apex_runtime::armed::threshold::{band, best_at, move_of, rungs, trigger_up, Pricing};
use apex_runtime::econ::RouteCosts;
use apex_runtime::live::frontier;
use apex_runtime::live::gas;
use apex_types::ids::ChainId;
use std::collections::BTreeMap;
use std::sync::Arc;

fn pair_and_cycles() -> (Vec<apex_runtime::live::frontier::Cycle>, BTreeMap<Address, Arc<PoolSnapshot>>) {
    let a = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let b = pool(SLIP, Venue::Slipstream, -197_350, 400);
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [(UNI, Arc::new(a)), (SLIP, Arc::new(b))].into_iter().collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    (cycles.into_values().collect(), snap)
}

fn px(margin: u128) -> Pricing {
    Pricing {
        costs: RouteCosts { other_wei: 20_000_000_000, wei_per_gas: 5_000_000 },
        gas: gas::MEASURED,
        cap: Some(U256::from(10u128.pow(21))),
        margin_wei: U256::from(margin),
    }
}

/// Two pools at the same price pay nothing; moving the trigger opens the gap
/// at a threshold, and one step less does not clear it.
#[test]
fn the_threshold_is_the_smallest_move_that_clears_the_margin() {
    let (cycles, snap) = pair_and_cycles();
    let c = &cycles[0];
    let pair = [Arc::clone(&snap[&c.legs[0].pool]), Arc::clone(&snap[&c.legs[1].pool])];
    let found = rungs(c, &pair, 0, &px(0), 3);
    assert!(!found.is_empty(), "a big enough move opens the gap");
    let first = &found[0];
    let up = trigger_up(c, 0);
    let one_less = pair[0].moved(move_of(&pair[0], up, first.step - 1)).unwrap();
    assert!(best_at(c, &pair, 0, one_less, &px(0)).is_none(), "minimal");
    // Rungs: 2x and 4x the move, each paying more than the one before.
    assert_eq!(found.iter().map(|r| r.step).collect::<Vec<_>>(), vec![first.step, 2 * first.step, 4 * first.step]);
    assert!(found.windows(2).all(|w| w[1].point.surplus > w[0].point.surplus));
    // A margin raises the threshold.
    let stricter = rungs(c, &pair, 0, &px(first.point.surplus.as_u128() * 2), 1);
    assert!(stricter[0].step > first.step);
}

/// The other pool's band holds the rung's fixed size above the margin at
/// both edges; a band is never wider than the cap.
#[test]
fn the_band_keeps_the_fixed_size_paying_at_both_edges() {
    let (cycles, snap) = pair_and_cycles();
    let c = &cycles[0];
    let pair = [Arc::clone(&snap[&c.legs[0].pool]), Arc::clone(&snap[&c.legs[1].pool])];
    let rung = rungs(c, &pair, 0, &px(0), 3).pop().unwrap(); // the largest: room for a band
    let [lo, hi] = band(c, &pair, 0, &rung, &px(0), 5).expect("a band");
    let other = &pair[1];
    assert!(lo.state.sqrt_price_x96 < other.state.sqrt_price_x96 && hi.state.sqrt_price_x96 > other.state.sqrt_price_x96);
    assert!(other.state.tick - lo.state.tick <= 5 && hi.state.tick - other.state.tick <= 5);
    assert!(band(c, &pair, 0, &rung, &px(rung.point.surplus.as_u128()), 5).is_none(), "no room at the margin");
}

#[test]
fn the_trigger_moves_against_the_cycles_own_swap() {
    let (cycles, _) = pair_and_cycles();
    for c in &cycles {
        for leg in 0..2 {
            assert_eq!(trigger_up(c, leg), c.legs[leg].zero_for_one);
        }
    }
    let _ = SearchBudget::default();
}
```

- [ ] **Step 2: Run, expect compile failures:** `cargo test -j 3 --offline -p apex-runtime --test armed_threshold`

- [ ] **Step 3: Implement.**

`crates/apex-runtime/src/live/pricing.rs`: split `LiveCycle::quote`'s loop body into `hop`, and add `quote_fixed`:

```rust
    /// One hop at `amount`: what it returns and the steps it takes. `None` if
    /// it cannot fill the whole input.
    fn hop(&self, i: usize, amount: U256) -> Option<(U256, HopSteps)> {
        let (pool, zero_for_one) = &self.legs[i];
        let (out, crossed, word_steps) = match &pool.reserves {
            Some(r) => (crate::live::cp::quote_out(r, pool.state.fee_ppm, amount, *zero_for_one)?, 0, 0),
            None => {
                let q = quote_exact_input_multi_tick(&pool.state, &pool.ladder, amount, *zero_for_one, MAX_TICKS)?;
                if q.exhausted || q.amount_in_consumed < amount {
                    return None;
                }
                (q.amount_out, q.ticks_crossed, q.word_steps)
            }
        };
        Some((out, HopSteps { venue: pool.spec.venue, zero_for_one: *zero_for_one, crossed, word_steps }))
    }

    /// Both hops at `amount_in`. `None` if either cannot fill it whole.
    pub fn quote(&self, amount_in: U256) -> Option<CycleQuote> {
        let (first, a) = self.hop(0, amount_in)?;
        let (second, b) = self.hop(1, first)?;
        Some(CycleQuote { outputs: [first, second], hops: [a, b] })
    }

    /// The hops as an armed plan runs them (R26): hop 1 sells `amount_in`, hop
    /// 2 sells exactly `mid`, its fixed input. `None` when hop 1 returns less
    /// than `mid` (the plan would revert) or either cannot fill.
    pub fn quote_fixed(&self, amount_in: U256, mid: U256) -> Option<CycleQuote> {
        let (first, a) = self.hop(0, amount_in)?;
        if first < mid {
            return None;
        }
        let (second, b) = self.hop(1, mid)?;
        Some(CycleQuote { outputs: [first, second], hops: [a, b] })
    }
```

and in `impl CostedCycle`: `pub fn cycle(&self) -> &LiveCycle { &self.cycle }`.

`crates/apex-runtime/src/armed/mod.rs`:

```rust
//! Armed backruns (Task 8.5 R26): transactions that wait in Base's builder
//! for a gap's condition (validity transactions). In armed-shadow they are
//! built and recorded, never sent; `apex armed-eval` judges them against the
//! chain.

pub mod engine;
pub mod eval;
pub mod lines;
pub mod moves;
pub mod plan;
pub mod predicate;
pub mod run;
pub mod threshold;
```

(Until Tasks 4–7 land, declare only the modules that exist.)

`crates/apex-runtime/src/armed/threshold.rs`:

```rust
//! Where an armed backrun's trigger sits, and what it trades there (R26).
//!
//! A slot is a cycle and the pool whose move opens its gap. The move goes
//! against the cycle's own swap on that pool. Profit at the best size grows
//! with the move, so the smallest move that clears the margin is found by
//! doubling and then bisecting; rungs sit at 2× and 4× it.

use crate::econ::{GasEstimate, RouteCosts, SettledRoute};
use crate::live::book::{Move, PoolSnapshot};
use crate::live::frontier::Cycle;
use crate::live::gas::SettlementGas;
use crate::live::pricing::{CostedCycle, LiveCycle};
use alloy_primitives::Address;
use apex_math::finite_size::{best_size, SearchBudget, Surplus};
use ethers_core::types::U256;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The furthest a trigger is searched: 2,000 ticks (about 20% of price) or
/// 2,000 bps.
pub const MAX_STEP: u32 = 2_000;

#[derive(Clone, Copy, Debug)]
pub struct Pricing {
    pub costs: RouteCosts,
    pub gas: SettlementGas,
    /// The lender's holding: the most any size may borrow.
    pub cap: Option<U256>,
    /// What the surplus over base costs must exceed (`armed.margin_wei`).
    pub margin_wei: U256,
}

/// The best size at one state, and what it would settle for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    pub size: U256,
    /// Hop 1's output at `size`: hop 2's fixed input.
    pub mid: U256,
    pub output: U256,
    /// Base gas and the L1 fee at `size`.
    pub cost: U256,
    /// `output − size − cost`.
    pub surplus: U256,
    pub gas: GasEstimate,
}

#[derive(Clone, Debug)]
pub struct Rung {
    /// 1, 2, 3: the threshold and its 2× and 4× moves.
    pub index: u8,
    /// The move, in ticks or bps.
    pub step: u32,
    pub point: Point,
    /// The trigger pool at this rung's threshold.
    pub trigger: PoolSnapshot,
}

/// The trigger moves against the cycle's own swap on its pool: up when the
/// cycle sells token0 there.
pub fn trigger_up(cycle: &Cycle, leg: usize) -> bool {
    cycle.legs[leg].zero_for_one
}

/// `step` ticks (or bps) from `p`'s price, up or down.
pub fn move_of(p: &PoolSnapshot, up: bool, step: u32) -> Move {
    if p.reserves.is_some() {
        Move::Bps { up, bps: step }
    } else {
        let s = i32::try_from(step).unwrap_or(i32::MAX);
        Move::Tick(if up { p.state.tick.saturating_add(s) } else { p.state.tick.saturating_sub(s) })
    }
}

fn map_of(cycle: &Cycle, pools: [Arc<PoolSnapshot>; 2]) -> BTreeMap<Address, Arc<PoolSnapshot>> {
    cycle.legs.iter().map(|l| l.pool).zip(pools).collect()
}

/// The best size with the trigger leg's pool replaced by `trigger`, if its
/// surplus exceeds the margin.
pub fn best_at(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, trigger: PoolSnapshot, px: &Pricing) -> Option<Point> {
    let mut pools = pair.clone();
    pools[leg] = Arc::new(trigger);
    let live = LiveCycle::new(cycle, &map_of(cycle, pools))?;
    let route = CostedCycle::new(live, px.costs, px.gas).capped(px.cap);
    let best = best_size(&route, SearchBudget::default()).ok()?;
    let Surplus::Gain(surplus) = best.net else { return None };
    if surplus <= px.margin_wei {
        return None;
    }
    let gas = route.gas_at(best.amount_in)?;
    let [mid, output] = route.cycle().hop_outputs(best.amount_in)?;
    let cost = U256::from(px.costs.with_gas(gas.expected));
    Some(Point { size: best.amount_in, mid, output, cost, surplus, gas })
}

fn at(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, px: &Pricing, step: u32) -> Option<(Point, PoolSnapshot)> {
    let moved = pair[leg].moved(move_of(&pair[leg], trigger_up(cycle, leg), step))?;
    Some((best_at(cycle, pair, leg, moved.clone(), px)?, moved))
}

/// The threshold and up to `count` rungs (the threshold, 2× and 4× its
/// move). Empty when no move within [`MAX_STEP`] clears the margin.
pub fn rungs(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, px: &Pricing, count: u8) -> Vec<Rung> {
    let mut hi = 1u32;
    let mut found = None;
    while hi <= MAX_STEP {
        if let Some(x) = at(cycle, pair, leg, px, hi) {
            found = Some(x);
            break;
        }
        hi = hi.saturating_mul(2);
    }
    let Some((mut point, mut trigger)) = found else { return Vec::new() };
    let mut lo = hi / 2;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        match at(cycle, pair, leg, px, mid) {
            Some((p, t)) => (hi, point, trigger) = (mid, p, t),
            None => lo = mid,
        }
    }
    let mut out = vec![Rung { index: 1, step: hi, point, trigger }];
    for index in 2..=count.min(3) {
        let step = hi.saturating_mul(1 << (index - 1));
        let Some((point, trigger)) = (step <= MAX_STEP).then(|| at(cycle, pair, leg, px, step)).flatten() else { break };
        out.push(Rung { index, step, point, trigger });
    }
    out
}

/// The other pool's band: the widest of 5, 2 and 1 steps (at most
/// `cap_steps`) at which the rung's fixed size, with the trigger at its
/// threshold, still clears the margin at both edges. `[lower, upper]`.
pub fn band(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, rung: &Rung, px: &Pricing, cap_steps: u32) -> Option<[PoolSnapshot; 2]> {
    let other = 1 - leg;
    for j in [5u32, 2, 1].into_iter().filter(|j| *j <= cap_steps) {
        let (Some(lo), Some(hi)) =
            (pair[other].moved(move_of(&pair[other], false, j)), pair[other].moved(move_of(&pair[other], true, j)))
        else {
            continue;
        };
        if [&lo, &hi].iter().all(|edge| clears_fixed(cycle, pair, leg, rung, edge, px)) {
            return Some([lo, hi]);
        }
    }
    None
}

fn clears_fixed(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, rung: &Rung, edge: &PoolSnapshot, px: &Pricing) -> bool {
    let mut pools = pair.clone();
    pools[leg] = Arc::new(rung.trigger.clone());
    pools[1 - leg] = Arc::new(edge.clone());
    let Some(live) = LiveCycle::new(cycle, &map_of(cycle, pools)) else { return false };
    let Some(q) = live.quote_fixed(rung.point.size, rung.point.mid) else { return false };
    let cost = U256::from(px.costs.with_gas(px.gas.estimate(&q.hops).expected));
    q.outputs[1] > rung.point.size.saturating_add(cost).saturating_add(px.margin_wei)
}
```

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 --offline -p apex-runtime --test armed_threshold --test live_pricing`. If `the_band_keeps_the_fixed_size_paying_at_both_edges` finds no band at the largest rung, raise the rung count's move (the fixture's pools are identical, so the gap is the move alone) by moving pool `b` 30 ticks in the cycle's favour, and keep the assertion.

---

### Task 4: Predicates and the break-even plan

**Files:**
- Create: `crates/apex-runtime/src/armed/predicate.rs`, `crates/apex-runtime/src/armed/plan.rs`
- Modify: `crates/apex-runtime/src/live/calls.rs` (extract `encode_step`)
- Test: `crates/apex-runtime/tests/armed_predicate.rs`

**Interfaces:**
- Consumes: Task 3's `Rung`, band edges.
- Produces:
  - `predicate::{Layout, layout(Venue) -> Layout, price_mask(), liquidity_mask(), Op::{AtLeast, AtMost}, Predicate::{Storage{address, slot, mask, op, value}, BlockNumber{op, value}}, trigger(&PoolSnapshot, at: &PoolSnapshot, up: bool) -> Option<Vec<Predicate>>, band(&PoolSnapshot, edges: &[PoolSnapshot; 2], floor_bps: u32) -> Option<Vec<Predicate>>, expiry(to_block: u64) -> Predicate}`;
  - `live::calls::encode_step(leg: &Leg, pool: &PoolSnapshot, amount_in: U256 /*alloy*/, min_out: U256, executor: Address, deadline: u64) -> Result<Step, String>`;
  - `plan::armed_call(cycle: &Cycle, pools: [&PoolSnapshot; 2], size: ethers U256, mid: ethers U256, executor: Address, chain_id: u64, deadline: u64) -> Result<ExecutorCall, String>`.

- [ ] **Step 1: Write the failing tests** `crates/apex-runtime/tests/armed_predicate.rs` (copy the `pool` helper and constants from Task 2, and `SLIP`):

```rust
use apex_runtime::armed::plan::armed_call;
use apex_runtime::armed::predicate::{band, expiry, layout, trigger, Predicate};
use apex_runtime::live::calls::encode_step;
use apex_runtime::live::book::Move;
use apex_runtime::live::frontier;
use apex_types::ids::ChainId;

const EXECUTOR: Address = address!("8940B565D050974b2b589B70De43bCb753b1F93B");

/// The JSON is exactly what `base_sendRawTransactionValidity` takes.
#[test]
fn predicates_encode_as_the_rpc_takes_them() {
    let p = pool(SLIP, Venue::Slipstream, -197_350, 400);
    let at = p.moved(Move::Tick(-197_300)).unwrap();
    let preds = trigger(&p, &at, true).unwrap();
    let v = serde_json::to_value(&preds).unwrap();
    assert_eq!(v[0]["type"], "storage");
    assert_eq!(v[0]["params"]["address"], "0xdbc6998296caa1652a810dc8d3baf4a8294330f1");
    assert_eq!(v[0]["params"]["slot"], "0x6");
    assert_eq!(v[0]["params"]["mask"], "0xffffffffffffffffffffffffffffffffffffffff");
    assert_eq!(v[0]["params"]["op"], ">=");
    assert_eq!(v[0]["params"]["value"], format!("{:#x}", at.state.sqrt_price_x96));
    let e = serde_json::to_value(expiry(52_510_000)).unwrap();
    assert_eq!(e, serde_json::json!({"type": "block_number", "params": {"op": "<=", "value": "0x3213d30"}}));
    let back: Vec<Predicate> = serde_json::from_value(v).unwrap();
    assert_eq!(back, preds);
}

#[test]
fn each_venue_watches_its_verified_slots() {
    assert_eq!((layout(Venue::UniswapV3).price_slot, layout(Venue::UniswapV3).liquidity_slot), (Some(0), Some(4)));
    assert_eq!((layout(Venue::PancakeV3).price_slot, layout(Venue::PancakeV3).liquidity_slot), (Some(0), Some(5)));
    for v in [Venue::Slipstream, Venue::SlipstreamV3] {
        assert_eq!((layout(v).price_slot, layout(v).liquidity_slot), (Some(6), Some(16)));
    }
    assert_eq!(layout(Venue::AerodromeV2).reserve_slots, Some((20, 21)));
}

/// The other pool: a price band and a liquidity floor.
#[test]
fn the_band_is_two_bounds_and_a_floor() {
    let p = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let edges = [p.moved(Move::Tick(-197_355)).unwrap(), p.moved(Move::Tick(-197_345)).unwrap()];
    let v = serde_json::to_value(band(&p, &edges, 9_800).unwrap()).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 3);
    assert_eq!((v[0]["params"]["op"].as_str(), v[1]["params"]["op"].as_str()), (Some(">="), Some("<=")));
    assert_eq!(v[2]["params"]["slot"], "0x4");
    assert_eq!(v[2]["params"]["value"], format!("{:#x}", U256::from(L) * U256::from(9_800) / U256::from(10_000)));
}

/// Hop 1's floor is `mid`, hop 2's the loan plus one, the plan's profit floor 1.
#[test]
fn the_armed_plan_carries_break_even_floors() {
    let a = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let b = pool(SLIP, Venue::Slipstream, -197_300, 400);
    let snap = [(UNI, std::sync::Arc::new(a.clone())), (SLIP, std::sync::Arc::new(b.clone()))].into_iter().collect();
    let (_, cycles) = frontier::build(ChainId(8453), frontier::WETH, &snap);
    let c = cycles.values().next().unwrap();
    let (p0, p1) = if c.legs[0].pool == UNI { (&a, &b) } else { (&b, &a) };
    let (size, mid) = (U256::from(10u128.pow(18)), U256::from(3_000_000_000u64));
    let call = armed_call(c, [p0, p1], size, mid, EXECUTOR, 8453, 1_791_800_000).unwrap();
    let plan = call.plan();
    assert_eq!(plan.min_profit, alloy_primitives::U256::from(1));
    assert_eq!(plan.deadline, 1_791_800_000);
    let (s, m) = (apex_types::compat::u256_to_alloy(size), apex_types::compat::u256_to_alloy(mid));
    assert_eq!(plan.steps[0], encode_step(&c.legs[0], p0, s, m, EXECUTOR, 1_791_800_000).unwrap());
    assert_eq!(plan.steps[1], encode_step(&c.legs[1], p1, m, s + alloy_primitives::U256::from(1), EXECUTOR, 1_791_800_000).unwrap());
}
```

(If `PlanV2`'s field names differ from `min_profit`, `deadline` and `steps`, use the names `apex-exec/src/commitment.rs` declares; `Step` must derive `PartialEq` for the comparison. Add the derive if it is missing.)

- [ ] **Step 2: Run, expect compile failures:** `cargo test -j 3 --offline -p apex-runtime --test armed_predicate`

- [ ] **Step 3: Implement.**

`crates/apex-runtime/src/live/calls.rs`: move the body of `LiveCallBuilder::step` into a free function, with the book lookup and the commitment's fields as arguments, and keep the method as a wrapper:

```rust
/// One hop as the executor's step for its venue: shared by the reactive plan
/// and the armed one (R26). `pool` is the hop's pool as the book holds it, for
/// its fee and tick spacing.
pub fn encode_step(
    leg: &Leg,
    pool: &PoolSnapshot,
    amount_in: U256,
    min_out: U256,
    executor: Address,
    deadline: u64,
) -> Result<Step, String> {
    // ... the current `step` body, with `pool` for the book's snapshot,
    // `executor` for `k.executor_address`, `deadline` for `k.deadline`, and
    // `.map_err(|e| e.to_string())` in place of `refuse(...)` ...
}

impl LiveCallBuilder {
    fn step(&self, leg: &Leg, amount_in: U256, min_out: U256, k: &ExecutionCommitment) -> Result<Step, Decline> {
        let pool = self.book.get(leg.pool).ok_or_else(stale)?;
        encode_step(leg, &pool, amount_in, min_out, k.executor_address, k.deadline).map_err(refuse)
    }
}
```

Add `use crate::live::book::PoolSnapshot;` to `calls.rs`.

`crates/apex-runtime/src/armed/predicate.rs`:

```rust
//! The predicates an armed transaction carries (R26), as Base's
//! `base_sendRawTransactionValidity` takes them: raw storage words, masked
//! and compared, and a block bound. Layouts verified on chain, 2026-10-12.

use crate::live::book::PoolSnapshot;
use crate::live::inventory::Venue;
use alloy_primitives::Address;
use ethers_core::types::U256;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The word whose low 160 bits are sqrtPriceX96.
    pub price_slot: Option<u64>,
    /// The word whose low 128 bits are the in-range liquidity.
    pub liquidity_slot: Option<u64>,
    /// reserve0's and reserve1's words.
    pub reserve_slots: Option<(u64, u64)>,
}

pub const fn layout(v: Venue) -> Layout {
    match v {
        Venue::UniswapV3 => Layout { price_slot: Some(0), liquidity_slot: Some(4), reserve_slots: None },
        Venue::PancakeV3 => Layout { price_slot: Some(0), liquidity_slot: Some(5), reserve_slots: None },
        Venue::Slipstream | Venue::SlipstreamV3 => Layout { price_slot: Some(6), liquidity_slot: Some(16), reserve_slots: None },
        Venue::AerodromeV2 => Layout { price_slot: None, liquidity_slot: None, reserve_slots: Some((20, 21)) },
    }
}

pub fn price_mask() -> U256 {
    (U256::one() << 160) - U256::one()
}

pub fn liquidity_mask() -> U256 {
    (U256::one() << 128) - U256::one()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    #[serde(rename = ">=")]
    AtLeast,
    #[serde(rename = "<=")]
    AtMost,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "params", rename_all = "snake_case")]
pub enum Predicate {
    Storage {
        address: String,
        slot: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<String>,
        op: Op,
        value: String,
    },
    BlockNumber { op: Op, value: String },
}

fn hex(v: U256) -> String {
    format!("{v:#x}")
}

fn storage(pool: Address, slot: u64, mask: Option<U256>, op: Op, value: U256) -> Predicate {
    Predicate::Storage { address: format!("{pool:#x}"), slot: format!("{slot:#x}"), mask: mask.map(hex), op, value: hex(value) }
}

/// The trigger past its threshold: `at` is the trigger pool at the threshold.
/// A constant-product pool needs both reserves bounded, so a mint (both rise)
/// cannot fire it.
pub fn trigger(p: &PoolSnapshot, at: &PoolSnapshot, up: bool) -> Option<Vec<Predicate>> {
    let l = layout(p.spec.venue);
    let pool = p.spec.pool;
    match (&at.reserves, l.reserve_slots, l.price_slot) {
        // token0's price in token1 up: reserve0 falls, reserve1 rises.
        (Some(r), Some((s0, s1)), _) if up => Some(vec![
            storage(pool, s0, None, Op::AtMost, r.reserve0),
            storage(pool, s1, None, Op::AtLeast, r.reserve1),
        ]),
        (Some(r), Some((s0, s1)), _) => Some(vec![
            storage(pool, s0, None, Op::AtLeast, r.reserve0),
            storage(pool, s1, None, Op::AtMost, r.reserve1),
        ]),
        (None, _, Some(slot)) => Some(vec![storage(
            pool,
            slot,
            Some(price_mask()),
            if up { Op::AtLeast } else { Op::AtMost },
            at.state.sqrt_price_x96,
        )]),
        _ => None,
    }
}

/// The other pool between `edges` (`[lower, upper]` in price), and its
/// liquidity at least `floor_bps` of what it holds.
pub fn band(p: &PoolSnapshot, edges: &[PoolSnapshot; 2], floor_bps: u32) -> Option<Vec<Predicate>> {
    let l = layout(p.spec.venue);
    let pool = p.spec.pool;
    let [lo, hi] = edges;
    match (&p.reserves, l.reserve_slots, l.price_slot, l.liquidity_slot) {
        (Some(_), Some((s0, s1)), _, _) => {
            let (a, b) = (lo.reserves.as_ref()?, hi.reserves.as_ref()?);
            Some(vec![
                storage(pool, s0, None, Op::AtLeast, a.reserve0.min(b.reserve0)),
                storage(pool, s0, None, Op::AtMost, a.reserve0.max(b.reserve0)),
                storage(pool, s1, None, Op::AtLeast, a.reserve1.min(b.reserve1)),
                storage(pool, s1, None, Op::AtMost, a.reserve1.max(b.reserve1)),
            ])
        }
        (None, _, Some(price), Some(liquidity)) => {
            let floor = U256::from(p.state.liquidity) * U256::from(floor_bps) / U256::from(10_000u32);
            Some(vec![
                storage(pool, price, Some(price_mask()), Op::AtLeast, lo.state.sqrt_price_x96),
                storage(pool, price, Some(price_mask()), Op::AtMost, hi.state.sqrt_price_x96),
                storage(pool, liquidity, Some(liquidity_mask()), Op::AtLeast, floor),
            ])
        }
        _ => None,
    }
}

/// No later than `to_block`.
pub fn expiry(to_block: u64) -> Predicate {
    Predicate::BlockNumber { op: Op::AtMost, value: format!("{to_block:#x}") }
}
```

`crates/apex-runtime/src/armed/plan.rs`:

```rust
//! An armed backrun's executor call (R26). Floors only stop a trade that
//! cannot repay its loan: once included, the fee is spent either way.

use crate::live::book::PoolSnapshot;
use crate::live::calls::encode_step;
use crate::live::frontier::{Cycle, BALANCER_VAULT};
use alloy_primitives::{Address, U256};
use apex_exec::call::ExecutorCall;
use apex_exec::commitment::{plan_commitment, Loan, LoanProvider};
use apex_exec::encode::PlanEncoder;
use apex_exec::sign::SignedPlan;
use apex_types::compat::u256_to_alloy;

/// The cycle at `size`: hop 1 must return `mid` (hop 2's fixed input), hop 2
/// the loan plus one, and the plan a profit of at least one.
pub fn armed_call(
    cycle: &Cycle,
    pools: [&PoolSnapshot; 2],
    size: ethers_core::types::U256,
    mid: ethers_core::types::U256,
    executor: Address,
    chain_id: u64,
    deadline: u64,
) -> Result<ExecutorCall, String> {
    let (size, mid, one) = (u256_to_alloy(size), u256_to_alloy(mid), U256::from(1));
    let steps = vec![
        encode_step(&cycle.legs[0], pools[0], size, mid, executor, deadline)?,
        encode_step(&cycle.legs[1], pools[1], mid, size.checked_add(one).ok_or("size overflows")?, executor, deadline)?,
    ];
    let loan = Loan { token: cycle.start, amount: size, provider: LoanProvider::Balancer, provider_addr: BALANCER_VAULT };
    let plan = PlanEncoder::build(loan, steps, 0, one, chain_id, deadline);
    let declared = plan_commitment(&plan, chain_id, executor);
    let checked = SignedPlan::check(plan, declared, chain_id, executor).map_err(|e| e.to_string())?;
    Ok(ExecutorCall::from_checked(checked))
}
```

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 --offline -p apex-runtime --test armed_predicate --test live_calls` (whatever the calls test file is named; the reactive builder's tests must still pass unchanged).

---

### Task 5: The arming engine, its ledger, config and report

**Files:**
- Create: `crates/apex-runtime/src/armed/lines.rs`, `crates/apex-runtime/src/armed/moves.rs`, `crates/apex-runtime/src/armed/engine.rs`
- Modify: `crates/apex-runtime/src/live/reads.rs` (`storage_at`), `crates/apex-runtime/src/shadow/config.rs` (`ArmedConfig`), `crates/apex-runtime/src/shadow/mod.rs` (boot check, worker, report), `ops/shadow.base.yaml`, `crates/apex-runtime/tests/shadow_parts.rs` (yaml), `scripts/shadow-status.sh`
- Test: `crates/apex-runtime/tests/armed_engine.rs`

**Interfaces:**
- Consumes: Tasks 3–4.
- Produces:
  - `lines::{ArmedLine::{Arm(Arm), Renew{arm_id, to_block, at_ns}}, Arm, Word}`;
  - `moves::MoveStats::{new(window_ns), observe(&mut self, pool, position: f64, now_ns), p_at_least(&self, pool, up, step) -> f64}`;
  - `engine::{Engine, EngineSettings, Tick, verify_layouts}`, with
    - `Engine::new(cycles: BTreeMap<RouteId, Cycle>, settings: EngineSettings, unarmable: BTreeSet<Address>) -> Engine`,
    - `Engine::tick(&self, snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>, head: u64, base_fee_wei: u128, px: &Pricing, now_ns: u64) -> Vec<ArmedLine>`,
    - `Engine::stats(&self) -> EngineStats`;
  - `ChainReads::storage_at(&self, a: Address, slot: u64, block: u64) -> Result<ethers U256, ReadError>`;
  - config `armed: ArmedConfig`.

- [ ] **Step 1: Write the failing tests** `crates/apex-runtime/tests/armed_engine.rs` (copy the `pool` helper, constants and `SLIP` from Task 2/3):

```rust
use apex_runtime::armed::engine::{Engine, EngineSettings};
use apex_runtime::armed::lines::ArmedLine;
use apex_runtime::armed::moves::MoveStats;
use apex_runtime::armed::threshold::Pricing;
use apex_runtime::econ::RouteCosts;
use apex_runtime::live::frontier;
use apex_runtime::live::gas;
use apex_types::ids::ChainId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

const EXECUTOR: Address = address!("8940B565D050974b2b589B70De43bCb753b1F93B");

fn setup(max_arms: usize) -> (Engine, BTreeMap<Address, Arc<PoolSnapshot>>) {
    let a = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let b = pool(SLIP, Venue::Slipstream, -197_350, 400);
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [(UNI, Arc::new(a)), (SLIP, Arc::new(b))].into_iter().collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let settings = EngineSettings {
        max_arms,
        rungs: 3,
        window_s: 10,
        block_time_ms: 2_000,
        band_cap_bps: 5,
        liquidity_floor_bps: 9_800,
        executor: EXECUTOR,
        chain_id: 8453,
    };
    (Engine::new(cycles, settings, BTreeSet::new()), snap)
}

fn px() -> Pricing {
    Pricing {
        costs: RouteCosts { other_wei: 20_000_000_000, wei_per_gas: 5_000_000 },
        gas: gas::MEASURED,
        cap: Some(U256::from(10u128.pow(21))),
        margin_wei: U256::zero(),
    }
}

#[test]
fn the_engine_arms_its_best_slots_and_renews_them() {
    let (engine, snap) = setup(4);
    let first = engine.tick(&snap, 1_000, 5_000_000, &px(), 1);
    let arms: Vec<_> = first.iter().filter_map(|l| if let ArmedLine::Arm(a) = l { Some(a) } else { None }).collect();
    assert!(!arms.is_empty() && arms.len() <= 4, "{}", arms.len());
    for a in &arms {
        assert_eq!(a.to_block, 1_005, "10 s of 2 s blocks");
        assert!(a.predicates.iter().any(|p| matches!(p, apex_runtime::armed::predicate::Predicate::BlockNumber { .. })));
        assert!(a.calldata.starts_with("0x") && a.calldata.len() > 10);
        assert!(a.sender_slot < 4);
    }
    let senders: BTreeSet<_> = arms.iter().map(|a| a.sender_slot).collect();
    assert_eq!(senders.len(), arms.len(), "one sender per live arm");
    // Same head, same book: nothing new.
    assert!(engine.tick(&snap, 1_000, 5_000_000, &px(), 2).is_empty());
    // Past every window, unchanged: renewed, not re-armed.
    let later = engine.tick(&snap, 1_006, 5_000_000, &px(), 3);
    assert!(!later.is_empty() && later.iter().all(|l| matches!(l, ArmedLine::Renew { .. })));
}

#[test]
fn a_line_round_trips_through_json() {
    let (engine, snap) = setup(1);
    let lines = engine.tick(&snap, 1_000, 5_000_000, &px(), 1);
    let text = serde_json::to_string(&lines[0]).unwrap();
    let back: ArmedLine = serde_json::from_str(&text).unwrap();
    assert_eq!(back, lines[0]);
}

#[test]
fn moves_are_counted_over_windows() {
    let mut m = MoveStats::new(10_000_000_000);
    let s = 1_000_000_000u64;
    // Two windows: one moving up 30, one flat.
    for (t, x) in [(0, 0.0), (5, 30.0), (10, 30.0), (19, 30.0), (21, 30.0)] {
        m.observe(UNI, x, t * s);
    }
    assert!(m.p_at_least(UNI, true, 20) > m.p_at_least(UNI, true, 40));
    assert!(m.p_at_least(UNI, false, 1) < 0.5);
}
```

In `shadow_parts.rs`'s yaml, after the `flashblocks:` block, add:

```yaml
armed:
  mode: shadow
  ledger: var/a.jsonl
  verdicts: var/v.jsonl
  max_arms: 16
  rungs: 3
  window_s: 10
  block_time_ms: 2000
  margin_wei: 0
  band_cap_bps: 5
  liquidity_floor_bps: 9800
```

and assert `c.armed.max_arms == 16` in its config test. Add a test that `mode: live` is refused:

```rust
#[test]
fn armed_live_is_refused_until_the_pilot_is_built() {
    let var = "APEX_SECRET_SHADOW_TEST_LIVE";
    let text = yaml(var, "j").replace("mode: shadow", "mode: live");
    let err = ShadowConfig::from_yaml_str(&text, &env(var, SECRET)).unwrap_err();
    assert!(err.to_string().contains("armed.mode live"), "{err}");
}
```

- [ ] **Step 2: Run, expect compile failures:** `cargo test -j 3 --offline -p apex-runtime --test armed_engine --test shadow_parts`

- [ ] **Step 3: Implement.**

`crates/apex-runtime/src/armed/lines.rs`:

```rust
//! The armed ledger's lines (R26): one per arm, and one per renewal of an arm
//! whose window ended with nothing changed. Written by the engine, read by
//! `apex armed-eval`. Amounts are decimal strings, words 0x-hex.

use super::predicate::Predicate;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArmedLine {
    Arm(Arm),
    Renew { arm_id: u64, to_block: u64, at_ns: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Word {
    pub address: String,
    pub slot: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arm {
    pub arm_id: u64,
    pub at_ns: u64,
    pub armed_block: u64,
    pub to_block: u64,
    pub route_hash: String,
    pub trigger_leg: u8,
    pub rung: u8,
    pub step: u32,
    /// The cycle's two pools, leg order, lowercase 0x.
    pub pools: [String; 2],
    pub venues: [String; 2],
    pub predicates: Vec<Predicate>,
    /// Every watched word's value when armed: the evaluator's starting state.
    pub start_words: Vec<Word>,
    /// The concentrated pools' ticks when armed, for Mint/Burn ranges.
    pub start_ticks: Vec<(String, i32)>,
    pub size_wei: String,
    pub mid: String,
    pub output_wei: String,
    pub surplus_wei: String,
    pub cost_wei: String,
    pub gas_expected: u64,
    pub gas_limit: u64,
    pub base_fee_wei: String,
    pub other_wei: String,
    pub deadline: u64,
    pub calldata: String,
    pub calldata_hash: String,
    pub sender_slot: u16,
}
```

`crates/apex-runtime/src/armed/moves.rs`:

```rust
//! How far each pool's price moves within an arming window (R26), learned
//! from the book as it runs: the ranking's `P(move ≥ m within W)`.
//!
//! A pool's position is its tick (1 tick ≈ 1 bp) or, for a constant-product
//! pool, its log-price in bps. Statistics only; nothing priced reads them.

use alloy_primitives::Address;
use std::collections::{BTreeMap, VecDeque};

/// A day of 10-second windows.
const KEEP: usize = 8_640;

#[derive(Debug, Default)]
struct PoolMoves {
    window_start_ns: u64,
    start: f64,
    max_up: f64,
    max_down: f64,
    windows: VecDeque<(f64, f64)>,
}

#[derive(Debug)]
pub struct MoveStats {
    window_ns: u64,
    pools: BTreeMap<Address, PoolMoves>,
}

impl MoveStats {
    pub fn new(window_ns: u64) -> Self {
        Self { window_ns: window_ns.max(1), pools: BTreeMap::new() }
    }

    pub fn observe(&mut self, pool: Address, position: f64, now_ns: u64) {
        let w = self.window_ns;
        let p = self.pools.entry(pool).or_insert_with(|| PoolMoves { window_start_ns: now_ns, start: position, ..Default::default() });
        if now_ns >= p.window_start_ns.saturating_add(w) {
            p.windows.push_back((p.max_up, p.max_down));
            if p.windows.len() > KEEP {
                p.windows.pop_front();
            }
            *p = PoolMoves { window_start_ns: now_ns, start: position, windows: std::mem::take(&mut p.windows), ..Default::default() };
        }
        p.max_up = p.max_up.max(position - p.start);
        p.max_down = p.max_down.max(p.start - position);
    }

    /// The share of closed windows in which `pool` moved at least `step`
    /// that way, Laplace-smoothed: (hits + 1) / (windows + 2).
    pub fn p_at_least(&self, pool: Address, up: bool, step: u32) -> f64 {
        let Some(p) = self.pools.get(&pool) else { return 0.5 };
        let hits = p.windows.iter().filter(|(u, d)| if up { *u } else { *d } >= f64::from(step)).count();
        (hits as f64 + 1.0) / (p.windows.len() as f64 + 2.0)
    }
}
```

`crates/apex-runtime/src/armed/engine.rs`:

```rust
//! The arming engine (R26): for every cycle and trigger pool, the rungs and
//! their predicates and calldata; the best `max_arms` armed, one virtual
//! sender each; a line per arm and per renewal. Pure: the shadow worker calls
//! [`Engine::tick`] and writes what it returns.

use super::lines::{Arm, ArmedLine, Word};
use super::moves::MoveStats;
use super::plan::armed_call;
use super::predicate::{self, layout};
use super::threshold::{band, rungs, trigger_up, Pricing, Rung};
use crate::live::book::PoolSnapshot;
use crate::live::frontier::Cycle;
use crate::live::reads::ChainReads;
use alloy_primitives::{keccak256, Address};
use apex_search::frontier::RouteId;
use ethers_core::types::U256;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

/// Slots recomputed per tick at most: the rest wait for the next 200 ms.
const RECOMPUTE_BUDGET: usize = 24;

#[derive(Clone, Copy, Debug)]
pub struct EngineSettings {
    pub max_arms: usize,
    pub rungs: u8,
    pub window_s: u64,
    pub block_time_ms: u64,
    pub band_cap_bps: u32,
    pub liquidity_floor_bps: u32,
    pub executor: Address,
    pub chain_id: u64,
}

impl EngineSettings {
    pub fn window_blocks(&self) -> u64 {
        (self.window_s * 1_000).div_ceil(self.block_time_ms.max(1)).max(1)
    }
}

type Key = (RouteId, u8, u8); // cycle, trigger leg, rung

/// A slot's rungs as last computed, with the pool versions they were priced at.
struct Computed {
    seqs: [u64; 2],
    at_ns: u64,
    rungs: Vec<(Rung, [PoolSnapshot; 2])>,
}

struct Live {
    arm: Arm,
    /// What decides "unchanged": step, size, mid and the band's prices.
    fingerprint: String,
}

#[derive(Default)]
struct State {
    moves: Option<MoveStats>,
    computed: BTreeMap<(RouteId, u8), Computed>,
    live: BTreeMap<Key, Live>,
    /// Each virtual sender's busy-until block.
    senders: Vec<u64>,
    next_arm_id: u64,
    cursor: usize,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct EngineStats {
    pub arms: u64,
    pub renewals: u64,
    pub slots_priced: u64,
    pub slots_armable: u64,
    pub unarmable_pools: u64,
    pub last_tick_us: u64,
}

pub struct Engine {
    cycles: BTreeMap<RouteId, Cycle>,
    settings: EngineSettings,
    unarmable: BTreeSet<Address>,
    state: Mutex<State>,
    arms: AtomicU64,
    renewals: AtomicU64,
    priced: AtomicU64,
    armable: AtomicU64,
    last_tick_us: AtomicU64,
}

fn position(p: &PoolSnapshot) -> f64 {
    match &p.reserves {
        Some(r) if !r.reserve0.is_zero() => {
            (r.reserve1.as_u128() as f64 / r.reserve0.as_u128() as f64).ln() * 10_000.0
        }
        _ => f64::from(p.state.tick),
    }
}

/// The words an arm's predicates watch, as `p` holds them now.
fn words_of(p: &PoolSnapshot) -> Vec<Word> {
    let l = layout(p.spec.venue);
    let a = format!("{:#x}", p.spec.pool);
    let w = |slot: u64, v: U256| Word { address: a.clone(), slot: format!("{slot:#x}"), value: format!("{v:#x}") };
    match (&p.reserves, l.reserve_slots) {
        (Some(r), Some((s0, s1))) => vec![w(s0, r.reserve0), w(s1, r.reserve1)],
        _ => [l.price_slot.map(|s| w(s, p.state.sqrt_price_x96)), l.liquidity_slot.map(|s| w(s, U256::from(p.state.liquidity)))]
            .into_iter()
            .flatten()
            .collect(),
    }
}

impl Engine {
    pub fn new(cycles: BTreeMap<RouteId, Cycle>, settings: EngineSettings, unarmable: BTreeSet<Address>) -> Self {
        let state = State { senders: vec![0; settings.max_arms], ..Default::default() };
        Self {
            cycles,
            settings,
            unarmable,
            state: Mutex::new(state),
            arms: AtomicU64::new(0),
            renewals: AtomicU64::new(0),
            priced: AtomicU64::new(0),
            armable: AtomicU64::new(0),
            last_tick_us: AtomicU64::new(0),
        }
    }

    pub fn stats(&self) -> EngineStats {
        EngineStats {
            arms: self.arms.load(Relaxed),
            renewals: self.renewals.load(Relaxed),
            slots_priced: self.priced.load(Relaxed),
            slots_armable: self.armable.load(Relaxed),
            unarmable_pools: self.unarmable.len() as u64,
            last_tick_us: self.last_tick_us.load(Relaxed),
        }
    }

    /// One pass: observe moves, reprice stale slots, arm the best, renew the
    /// unchanged. Returns the lines to write.
    pub fn tick(
        &self,
        snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>,
        head: u64,
        base_fee_wei: u128,
        px: &Pricing,
        now_ns: u64,
    ) -> Vec<ArmedLine> {
        let started = std::time::Instant::now();
        let s = self.settings;
        let window_ns = s.window_s * 1_000_000_000;
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let st = &mut *st;
        let moves = st.moves.get_or_insert_with(|| MoveStats::new(window_ns));
        for p in snapshot.values() {
            moves.observe(p.spec.pool, position(p), now_ns);
        }

        // Reprice stale slots, stalest first, within the budget.
        let mut stale: Vec<((RouteId, u8), u64)> = Vec::new();
        for (id, c) in &self.cycles {
            if c.legs.iter().any(|l| self.unarmable.contains(&l.pool)) {
                continue;
            }
            let (Some(a), Some(b)) = (snapshot.get(&c.legs[0].pool), snapshot.get(&c.legs[1].pool)) else { continue };
            for leg in 0..2u8 {
                let fresh = st.computed.get(&(*id, leg)).is_some_and(|k| {
                    k.seqs == [a.seq, b.seq] && now_ns < k.at_ns.saturating_add(window_ns)
                });
                if !fresh {
                    let age = st.computed.get(&(*id, leg)).map_or(0, |k| k.at_ns);
                    stale.push(((*id, leg), age));
                }
            }
        }
        stale.sort_by_key(|(_, age)| *age);
        for ((id, leg), _) in stale.into_iter().take(RECOMPUTE_BUDGET) {
            let c = &self.cycles[&id];
            let pair = [Arc::clone(&snapshot[&c.legs[0].pool]), Arc::clone(&snapshot[&c.legs[1].pool])];
            let found: Vec<(Rung, [PoolSnapshot; 2])> = rungs(c, &pair, usize::from(leg), px, s.rungs)
                .into_iter()
                .filter_map(|r| band(c, &pair, usize::from(leg), &r, px, s.band_cap_bps).map(|b| (r, b)))
                .collect();
            self.priced.fetch_add(1, Relaxed);
            st.computed.insert((id, leg), Computed { seqs: [pair[0].seq, pair[1].seq], at_ns: now_ns, rungs: found });
        }
        self.armable.store(st.computed.values().filter(|k| !k.rungs.is_empty()).count() as u64, Relaxed);

        // Rank every priced rung: P(move ≥ step) × surplus.
        let moves = st.moves.as_ref().expect("set above");
        let mut ranked: Vec<(f64, Key)> = Vec::new();
        for ((id, leg), k) in &st.computed {
            let c = &self.cycles[id];
            let pool = c.legs[usize::from(*leg)].pool;
            for (r, _) in &k.rungs {
                let p = moves.p_at_least(pool, trigger_up(c, usize::from(*leg)), r.step);
                ranked.push((p * r.point.surplus.as_u128() as f64, (*id, *leg, r.index)));
            }
        }
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));

        // Free the senders whose arms' windows have passed.
        for busy in &mut st.senders {
            if *busy < head {
                *busy = 0;
            }
        }
        let to_block = head + s.window_blocks();
        let mut out = Vec::new();
        for (_, key) in ranked.into_iter().take(s.max_arms) {
            let (id, leg, index) = key;
            let c = &self.cycles[&id];
            let Some((rung, edges)) = st.computed[&(id, leg)].rungs.iter().find(|(r, _)| r.index == index) else { continue };
            let fingerprint = format!("{}:{:#x}:{:#x}:{:#x}:{:#x}", rung.step, rung.point.size, rung.point.mid,
                edges[0].state.sqrt_price_x96, edges[1].state.sqrt_price_x96);
            if let Some(live) = st.live.get_mut(&key) {
                if live.fingerprint == fingerprint {
                    if live.arm.to_block < head {
                        live.arm.to_block = to_block;
                        st.senders[usize::from(live.arm.sender_slot)] = to_block;
                        self.renewals.fetch_add(1, Relaxed);
                        out.push(ArmedLine::Renew { arm_id: live.arm.arm_id, to_block, at_ns: now_ns });
                    }
                    continue;
                }
                if live.arm.to_block >= head {
                    continue; // changed, but its sender is still busy until expiry
                }
            }
            let Some(sender) = st.senders.iter().position(|b| *b == 0) else { break };
            let Some(arm) = self.arm(c, id, leg, rung, edges, snapshot, head, to_block, base_fee_wei, px, now_ns, sender, st.next_arm_id) else { continue };
            st.next_arm_id += 1;
            st.senders[sender] = to_block;
            self.arms.fetch_add(1, Relaxed);
            out.push(ArmedLine::Arm(arm.clone()));
            st.live.insert(key, Live { arm, fingerprint });
        }
        self.last_tick_us.store(started.elapsed().as_micros() as u64, Relaxed);
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn arm(
        &self,
        c: &Cycle,
        id: RouteId,
        leg: u8,
        rung: &Rung,
        edges: &[PoolSnapshot; 2],
        snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>,
        head: u64,
        to_block: u64,
        base_fee_wei: u128,
        px: &Pricing,
        now_ns: u64,
        sender: usize,
        arm_id: u64,
    ) -> Option<Arm> {
        let s = self.settings;
        let (t, o) = (usize::from(leg), 1 - usize::from(leg));
        let trigger_pool = &snapshot[&c.legs[t].pool];
        let other_pool = &snapshot[&c.legs[o].pool];
        let mut predicates = predicate::trigger(trigger_pool, &rung.trigger, trigger_up(c, t))?;
        predicates.extend(predicate::band(other_pool, edges, s.liquidity_floor_bps)?);
        predicates.push(predicate::expiry(to_block));
        let deadline = now_ns / 1_000_000_000 + s.window_s + 1;
        let pools = [&**snapshot.get(&c.legs[0].pool)?, &**snapshot.get(&c.legs[1].pool)?];
        let call = armed_call(c, pools, rung.point.size, rung.point.mid, s.executor, s.chain_id, deadline).ok()?;
        let data = call.data();
        let start_ticks = [trigger_pool, other_pool]
            .iter()
            .filter(|p| p.reserves.is_none())
            .map(|p| (format!("{:#x}", p.spec.pool), p.state.tick))
            .collect();
        let _ = id;
        Some(Arm {
            arm_id,
            at_ns: now_ns,
            armed_block: head,
            to_block,
            route_hash: format!("{:#x}", c.commitment.route_hash),
            trigger_leg: leg,
            rung: rung.index,
            step: rung.step,
            pools: [format!("{:#x}", c.legs[0].pool), format!("{:#x}", c.legs[1].pool)],
            venues: [format!("{:?}", c.legs[0].venue), format!("{:?}", c.legs[1].venue)],
            predicates,
            start_words: [words_of(trigger_pool), words_of(other_pool)].concat(),
            start_ticks,
            size_wei: rung.point.size.to_string(),
            mid: rung.point.mid.to_string(),
            output_wei: rung.point.output.to_string(),
            surplus_wei: rung.point.surplus.to_string(),
            cost_wei: rung.point.cost.to_string(),
            gas_expected: rung.point.gas.expected.0,
            gas_limit: rung.point.gas.ceiling.0.saturating_mul(5) / 4,
            base_fee_wei: base_fee_wei.to_string(),
            other_wei: px.costs.other_wei.to_string(),
            deadline,
            calldata: format!("0x{}", alloy_primitives::hex::encode(data)),
            calldata_hash: format!("{:#x}", keccak256(data)),
            sender_slot: u16::try_from(sender).unwrap_or(u16::MAX),
        })
    }
}

/// Every pool whose watched words do not read back, at `block`, as the book
/// loaded them: their arms would watch the wrong storage, so they are left
/// out (R26). Run at boot, on the freshly loaded book.
pub async fn verify_layouts(reads: &ChainReads, snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>, block: u64) -> BTreeSet<Address> {
    let mut bad = BTreeSet::new();
    for p in snapshot.values() {
        let l = layout(p.spec.venue);
        let a = p.spec.pool;
        let ok = match (&p.reserves, l.reserve_slots) {
            (Some(r), Some((s0, s1))) => {
                matches!((reads.storage_at(a, s0, block).await, reads.storage_at(a, s1, block).await),
                    (Ok(w0), Ok(w1)) if w0 == r.reserve0 && w1 == r.reserve1)
            }
            _ => match (l.price_slot, l.liquidity_slot) {
                (Some(ps), Some(ls)) => matches!(
                    (reads.storage_at(a, ps, block).await, reads.storage_at(a, ls, block).await),
                    (Ok(w), Ok(lw)) if (w & predicate::price_mask()) == p.state.sqrt_price_x96
                        && (lw & predicate::liquidity_mask()) == U256::from(p.state.liquidity)
                ),
                _ => false,
            },
        };
        if !ok {
            bad.insert(a);
        }
    }
    bad
}
```

`crates/apex-runtime/src/live/reads.rs`, in `impl ChainReads`:

```rust
    /// One storage word of `a` at `block` (R26: the armed layouts' check).
    pub async fn storage_at(&self, a: Address, slot: u64, block: u64) -> Result<ethers_core::types::U256, ReadError> {
        let v = self
            .rpc
            .call("eth_getStorageAt", json!([format!("{a:#x}"), format!("{slot:#x}"), format!("{block:#x}")]))
            .await?;
        v.as_str()
            .and_then(|s| ethers_core::types::U256::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .ok_or_else(|| ReadError::Malformed(format!("eth_getStorageAt: {v}")))
    }
```

`crates/apex-runtime/src/shadow/config.rs`: add to `Raw` and `ShadowConfig` an `armed: ArmedConfig` field (mapped through), the types below, and in validation:

```rust
/// R26: armed backruns. Only `shadow` is built: `live` is the pilot's plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmedMode {
    Shadow,
    Live,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmedConfig {
    pub mode: ArmedMode,
    pub ledger: PathBuf,
    pub verdicts: PathBuf,
    pub max_arms: usize,
    pub rungs: u8,
    pub window_s: u64,
    pub block_time_ms: u64,
    pub margin_wei: u64,
    pub band_cap_bps: u32,
    pub liquidity_floor_bps: u32,
}
```

```rust
        let a = &raw.armed;
        if a.mode == ArmedMode::Live {
            return Err(invalid("armed.mode live is not built yet: the pilot is R26's second plan, after gate A"));
        }
        if !(1..=64).contains(&a.max_arms) || !(1..=3).contains(&a.rungs) || !(2..=50).contains(&a.window_s)
            || a.block_time_ms == 0 || a.band_cap_bps == 0 || a.liquidity_floor_bps > 10_000
        {
            return Err(invalid("armed: max_arms 1-64, rungs 1-3, window_s 2-50, block_time_ms > 0, band_cap_bps > 0, liquidity_floor_bps ≤ 10000"));
        }
```

`ops/shadow.base.yaml`, after the `flashblocks:` block:

```yaml
# R26: armed backruns. armed-shadow arms each top route's gap as a validity
# transaction would wait in Base's builder: built and recorded, never sent.
# `apex armed-eval` (a timer on the host) judges every arm against the chain.
armed:
  mode: shadow
  ledger: var/apex/armed.jsonl
  verdicts: var/apex/armed-verdicts.jsonl
  max_arms: 16
  rungs: 3
  window_s: 10
  block_time_ms: 2000
  margin_wei: 0
  band_cap_bps: 5
  liquidity_floor_bps: 9800
```

`crates/apex-runtime/src/shadow/mod.rs`:
- in `boot`, right after `PoolBook::load` and before anything subscribes: `let unarmable = crate::armed::engine::verify_layouts(&reads, &book.snapshot(), head).await;` with a `warn!` per address, then build the engine from a clone of `cycles` taken before `cycles` is moved into the pricer:

```rust
    let armed = Arc::new(crate::armed::engine::Engine::new(
        cycles.clone(),
        crate::armed::engine::EngineSettings {
            max_arms: config.armed.max_arms,
            rungs: config.armed.rungs,
            window_s: config.armed.window_s,
            block_time_ms: config.armed.block_time_ms,
            band_cap_bps: config.armed.band_cap_bps,
            liquidity_floor_bps: config.armed.liquidity_floor_bps,
            executor: config.executor.address,
            chain_id: base.0,
        },
        unarmable,
    ));
```

- `Shadow` gains `armed: Arc<crate::armed::engine::Engine>`, and `Stats` gains `armed_write_failures: AtomicU64`;
- a new `Task::Armed` spawned with the others (`("armed", Task::Armed)`), dispatched to:

```rust
    /// R26: armed-shadow. Every 200 ms, the engine's pass over the verified
    /// book, off the async threads; its lines to the armed ledger.
    async fn armed_loop(&self) {
        let mut every = tokio::time::interval(Duration::from_millis(200));
        loop {
            every.tick().await;
            if self.book.status() != ReconstructionStatus::Verified {
                continue;
            }
            let Some(head) = self.heads.borrow().clone() else { continue };
            let px = crate::armed::threshold::Pricing {
                costs: self.costs.route_costs(),
                gas: gas::MEASURED,
                cap: *self.lender.borrow(),
                margin_wei: ethers_core::types::U256::from(self.config.armed.margin_wei),
            };
            let (engine, snapshot) = (Arc::clone(&self.armed), self.book.snapshot());
            let base_fee = head.base_fee_per_gas.unwrap_or(0);
            let now = SystemClock.now().0;
            let lines = tokio::task::spawn_blocking(move || engine.tick(&snapshot, head.number, base_fee, &px, now))
                .await
                .unwrap_or_default();
            for line in &lines {
                if ledger::append_line(&self.config.armed.ledger, now, line).is_err() {
                    bump(&self.stats.armed_write_failures);
                }
            }
        }
    }
```

- the `Report` gains `armed: ArmedReport { mode: "shadow", engine: EngineStats, write_failures: u64, move_checks: u64, move_mismatches: u64 }`, from `self.armed.stats()`, the stat and `self.book.move_checks()`.

`scripts/shadow-status.sh`, after the `race` line:

```python
        am = r.get("armed")
        if am:  # R26
            e = am["engine"]
            print(f"  armed       {e['arms']} arms, {e['renewals']} renewals, {e['slots_armable']} armable slots, "
                  f"{e['unarmable_pools']} unarmable pools, last pass {e['last_tick_us'] / 1000:.1f} ms, "
                  f"{am['write_failures']} write failures")
            print(f"  moves       {am['move_checks']} swaps checked against the move model, {am['move_mismatches']} mismatched")
```

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 --offline -p apex-runtime` (whole crate). Then `bash -n scripts/shadow-status.sh`.

---

### Task 6: The evaluator's judging (pure)

**Files:**
- Create: `crates/apex-runtime/src/armed/eval.rs`
- Test: `crates/apex-runtime/tests/armed_eval.rs`

**Interfaces:**
- Consumes: Task 5's `Arm`, `Word`; Task 4's `Predicate`.
- Produces:
  - `eval::{TxEffect{block, tx_index, tx, writes}, Write::{Set{address, slot, value}, Tick{address, tick}, Liquidity{address, slot, lower, upper, delta}}, Words, Point{block, tx_index, tx}, Judged{fire, rival}, judge(&Arm, &[TxEffect]) -> Option<Judged>, merge_renewals(Vec<ArmedLine>) -> Vec<Arm>, Landing{optimistic, pessimistic}, landing(fire_fb, rival_fb: Option<(u64,u32)>, bid, rival_bid) -> Landing, next_flashblock((u64,u32)) -> (u64,u32), bid_wei_per_gas(surplus: U256, gas: u64, f_permille: u32) -> u128, net_wei(gross: U256, gas: u64, base_fee: u128, bid: u128, other_wei: u128) -> i128, F_PERMILLE}`.

- [ ] **Step 1: Write the failing tests** `crates/apex-runtime/tests/armed_eval.rs`:

```rust
use apex_runtime::armed::eval::{
    bid_wei_per_gas, judge, landing, merge_renewals, net_wei, next_flashblock, Point, TxEffect, Write, F_PERMILLE,
};
use apex_runtime::armed::lines::{Arm, ArmedLine, Word};
use apex_runtime::armed::predicate::{expiry, Op, Predicate};
use ethers_core::types::U256;

const A: &str = "0x00000000000000000000000000000000000000aa";
const B: &str = "0x00000000000000000000000000000000000000bb";

fn storage(addr: &str, slot: &str, op: Op, v: u64) -> Predicate {
    Predicate::Storage { address: addr.into(), slot: slot.into(), mask: None, op, value: format!("{v:#x}") }
}

fn arm() -> Arm {
    Arm {
        arm_id: 7,
        at_ns: 0,
        armed_block: 100,
        to_block: 105,
        route_hash: "0xroute".into(),
        trigger_leg: 0,
        rung: 1,
        step: 10,
        pools: [A.into(), B.into()],
        venues: ["UniswapV3".into(), "UniswapV3".into()],
        // A's price at least 50; B between 9 and 11 with liquidity at least 900.
        predicates: vec![
            storage(A, "0x0", Op::AtLeast, 50),
            storage(B, "0x0", Op::AtLeast, 9),
            storage(B, "0x0", Op::AtMost, 11),
            storage(B, "0x4", Op::AtLeast, 900),
            expiry(105),
        ],
        start_words: vec![
            Word { address: A.into(), slot: "0x0".into(), value: "0x28".into() }, // 40
            Word { address: B.into(), slot: "0x0".into(), value: "0xa".into() },  // 10
            Word { address: B.into(), slot: "0x4".into(), value: "0x3e8".into() }, // 1000
        ],
        start_ticks: vec![(B.into(), 0)],
        size_wei: "1000".into(),
        mid: "10".into(),
        output_wei: "1100".into(),
        surplus_wei: "90".into(),
        cost_wei: "10".into(),
        gas_expected: 300_000,
        gas_limit: 400_000,
        base_fee_wei: "5000000".into(),
        other_wei: "1".into(),
        deadline: 0,
        calldata: "0x".into(),
        calldata_hash: "0x".into(),
        sender_slot: 0,
    }
}

fn set(addr: &str, slot: u64, v: u64) -> Write {
    Write::Set { address: addr.into(), slot, value: U256::from(v) }
}

fn tx(block: u64, i: u64, writes: Vec<Write>) -> TxEffect {
    TxEffect { block, tx_index: i, tx: format!("0x{block}{i}"), writes }
}

#[test]
fn an_arm_fires_on_the_first_transaction_that_makes_every_predicate_hold() {
    let effects = vec![
        tx(100, 3, vec![set(A, 0, 60)]),         // the arming block itself: not watched
        tx(101, 5, vec![set(A, 0, 45)]),         // not yet
        tx(102, 7, vec![set(A, 0, 55)]),         // fires
        tx(102, 8, vec![set(B, 0, 10)]),         // still holds
        tx(103, 1, vec![set(A, 0, 41)]),         // the rival: closes it
    ];
    let j = judge(&arm(), &effects).unwrap();
    assert_eq!(j.fire, Some(Point { block: 102, tx_index: 7, tx: "0x1027".into() }));
    assert_eq!(j.rival, Some(Point { block: 103, tx_index: 1, tx: "0x1031".into() }));
}

#[test]
fn the_other_pools_band_and_liquidity_gate_the_fire() {
    let effects = vec![tx(101, 1, vec![set(B, 0, 12)]), tx(101, 2, vec![set(A, 0, 55)])];
    assert_eq!(judge(&arm(), &effects).unwrap().fire, None, "B left its band");
    // A burn in range takes B's liquidity under its floor.
    let effects = vec![
        tx(101, 1, vec![Write::Liquidity { address: B.into(), slot: 4, lower: -10, upper: 10, delta: -200 }]),
        tx(101, 2, vec![set(A, 0, 55)]),
    ];
    assert_eq!(judge(&arm(), &effects).unwrap().fire, None, "1000 − 200 < 900");
    // Out of range, it does not count.
    let effects = vec![
        tx(101, 1, vec![Write::Liquidity { address: B.into(), slot: 4, lower: 20, upper: 30, delta: -200 }]),
        tx(101, 2, vec![set(A, 0, 55)]),
    ];
    assert!(judge(&arm(), &effects).unwrap().fire.is_some());
}

#[test]
fn nothing_after_the_window_counts() {
    let effects = vec![tx(106, 1, vec![set(A, 0, 55)])];
    assert_eq!(judge(&arm(), &effects).unwrap().fire, None);
}

#[test]
fn renewals_extend_an_arm_and_its_block_bound() {
    let lines = vec![ArmedLine::Arm(arm()), ArmedLine::Renew { arm_id: 7, to_block: 110, at_ns: 1 }];
    let merged = merge_renewals(lines);
    assert_eq!(merged[0].to_block, 110);
    assert!(merged[0].predicates.contains(&expiry(110)));
    assert!(!merged[0].predicates.contains(&expiry(105)));
}

#[test]
fn landing_follows_the_flashblock_rule() {
    let k = (102, 4);
    assert_eq!(next_flashblock((102, 10)), (103, 1));
    // No rival: won either way.
    let l = landing(k, None, 1, 0);
    assert!(l.optimistic && l.pessimistic);
    // Rival in the trigger's flashblock: optimistic needs the higher bid; pessimistic loses.
    let l = landing(k, Some((102, 4)), 10, 5);
    assert!(l.optimistic && !l.pessimistic);
    let l = landing(k, Some((102, 4)), 5, 10);
    assert!(!l.optimistic && !l.pessimistic);
    // Rival in the next: optimistic wins outright; pessimistic needs the higher bid.
    let l = landing(k, Some((102, 5)), 5, 10);
    assert!(l.optimistic && !l.pessimistic);
    let l = landing(k, Some((102, 5)), 10, 5);
    assert!(l.optimistic && l.pessimistic);
    // Rival later still: won either way.
    let l = landing(k, Some((102, 7)), 0, 1_000);
    assert!(l.optimistic && l.pessimistic);
}

#[test]
fn bids_and_nets_are_exact() {
    assert_eq!(F_PERMILLE, [50, 100, 200, 300, 500, 700, 900]);
    // 0.5 × 3e14 wei over 3e5 gas = 5e8 wei per gas.
    assert_eq!(bid_wei_per_gas(U256::from(300_000_000_000_000u64), 300_000, 500), 500_000_000);
    // gross 1e15 − 3e5 × (5e6 + 5e8) − 1e10
    assert_eq!(net_wei(U256::from(10u64.pow(15)), 300_000, 5_000_000, 500_000_000, 10_000_000_000), 848_490_000_000_000);
}
```

- [ ] **Step 2: Run, expect compile failures:** `cargo test -j 3 --offline -p apex-runtime --test armed_eval`

- [ ] **Step 3: Implement** `crates/apex-runtime/src/armed/eval.rs`:

```rust
//! Judging an arm against what the chain did (R26): when its predicates first
//! held, which transaction turned them false again (the rival), where each
//! landed, and what each bid would have netted.
//!
//! The words are rebuilt exactly: a Swap carries its pool's post-swap price,
//! tick and liquidity, a Sync its reserves, and a Mint or Burn moves the
//! in-range liquidity by its amount when the pool's tick is inside its range.

use super::lines::{Arm, ArmedLine};
use super::predicate::{Op, Predicate};
use ethers_core::types::U256;
use std::collections::BTreeMap;

/// The bid fractions swept, per mille.
pub const F_PERMILLE: [u32; 7] = [50, 100, 200, 300, 500, 700, 900];
/// Base's last flashblock index in a block; index 0 holds the block's system
/// transactions, so a block's first user flashblock is 1.
const LAST_INDEX: u32 = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    Set { address: String, slot: u64, value: U256 },
    Tick { address: String, tick: i32 },
    Liquidity { address: String, slot: u64, lower: i32, upper: i32, delta: i128 },
}

/// One transaction's writes to the watched pools, in chain order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxEffect {
    pub block: u64,
    pub tx_index: u64,
    pub tx: String,
    pub writes: Vec<Write>,
}

#[derive(Clone, Debug, Default)]
pub struct Words {
    values: BTreeMap<(String, u64), U256>,
    ticks: BTreeMap<String, i32>,
}

fn parse_hex(s: &str) -> Option<U256> {
    U256::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

fn parse_slot(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

impl Words {
    pub fn from_arm(arm: &Arm) -> Option<Self> {
        let mut w = Self::default();
        for word in &arm.start_words {
            w.values.insert((word.address.clone(), parse_slot(&word.slot)?), parse_hex(&word.value)?);
        }
        w.ticks = arm.start_ticks.iter().cloned().collect();
        Some(w)
    }

    pub fn apply(&mut self, write: &Write) {
        match write {
            Write::Set { address, slot, value } => {
                self.values.insert((address.clone(), *slot), *value);
            }
            Write::Tick { address, tick } => {
                self.ticks.insert(address.clone(), *tick);
            }
            Write::Liquidity { address, slot, lower, upper, delta } => {
                let in_range = self.ticks.get(address).is_some_and(|t| lower <= t && t < upper);
                if in_range {
                    if let Some(v) = self.values.get_mut(&(address.clone(), *slot)) {
                        let d = U256::from(delta.unsigned_abs());
                        *v = if *delta >= 0 { v.saturating_add(d) } else { v.saturating_sub(d) };
                    }
                }
            }
        }
    }

    /// Whether every predicate holds in `block`. A word never seen fails.
    pub fn holds(&self, predicates: &[Predicate], block: u64) -> bool {
        predicates.iter().all(|p| match p {
            Predicate::Storage { address, slot, mask, op, value } => {
                let (Some(slot), Some(want)) = (parse_slot(slot), parse_hex(value)) else { return false };
                let Some(word) = self.values.get(&(address.clone(), slot)) else { return false };
                let masked = match mask.as_deref().and_then(parse_hex) {
                    Some(m) => *word & m,
                    None => *word,
                };
                match op {
                    Op::AtLeast => masked >= want,
                    Op::AtMost => masked <= want,
                }
            }
            Predicate::BlockNumber { op, value } => {
                let want = parse_hex(value).map_or(0, |v| v.low_u64());
                match op {
                    Op::AtLeast => block >= want,
                    Op::AtMost => block <= want,
                }
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    pub block: u64,
    pub tx_index: u64,
    pub tx: String,
}

impl Point {
    fn of(e: &TxEffect) -> Self {
        Self { block: e.block, tx_index: e.tx_index, tx: e.tx.clone() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judged {
    pub fire: Option<Point>,
    pub rival: Option<Point>,
}

/// The fire point (the first transaction after which every predicate holds)
/// and the rival (the first after that which makes one false). `effects`
/// must hold every transaction that touched the arm's pools, in chain order.
pub fn judge(arm: &Arm, effects: &[TxEffect]) -> Option<Judged> {
    let mut words = Words::from_arm(arm)?;
    let mut fire = None;
    for e in effects.iter().filter(|e| e.block > arm.armed_block && e.block <= arm.to_block) {
        for w in &e.writes {
            words.apply(w);
        }
        let holds = words.holds(&arm.predicates, e.block);
        match (&fire, holds) {
            (None, true) => fire = Some(Point::of(e)),
            (Some(_), false) => return Some(Judged { fire, rival: Some(Point::of(e)) }),
            _ => {}
        }
    }
    Some(Judged { fire, rival: None })
}

/// Arms with their renewals folded in: the window and its block bound extended.
pub fn merge_renewals(lines: Vec<ArmedLine>) -> Vec<Arm> {
    let mut arms: BTreeMap<u64, Arm> = BTreeMap::new();
    for line in lines {
        match line {
            ArmedLine::Arm(a) => {
                arms.insert(a.arm_id, a);
            }
            ArmedLine::Renew { arm_id, to_block, .. } => {
                if let Some(a) = arms.get_mut(&arm_id) {
                    a.to_block = a.to_block.max(to_block);
                    for p in &mut a.predicates {
                        if let Predicate::BlockNumber { value, .. } = p {
                            *value = format!("{:#x}", a.to_block);
                        }
                    }
                }
            }
        }
    }
    arms.into_values().collect()
}

pub fn next_flashblock((block, index): (u64, u32)) -> (u64, u32) {
    if index >= LAST_INDEX {
        (block + 1, 1)
    } else {
        (block, index + 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Landing {
    /// We land in the trigger's flashblock.
    pub optimistic: bool,
    /// We land in the next one.
    pub pessimistic: bool,
}

/// Who lands first. A rival in a later flashblock than ours always loses;
/// in ours, the higher priority fee wins.
pub fn landing(fire: (u64, u32), rival: Option<(u64, u32)>, bid: u128, rival_bid: u128) -> Landing {
    let Some(r) = rival else { return Landing { optimistic: true, pessimistic: true } };
    let next = next_flashblock(fire);
    Landing {
        optimistic: r > fire || bid > rival_bid,
        pessimistic: if r < next { false } else if r == next { bid > rival_bid } else { true },
    }
}

/// `f` (per mille) of the surplus at the threshold, spread over the gas.
pub fn bid_wei_per_gas(surplus: U256, gas: u64, f_permille: u32) -> u128 {
    if gas == 0 {
        return 0;
    }
    (surplus * U256::from(f_permille) / U256::from(1_000u32) / U256::from(gas)).low_u128()
}

/// What a landed arm nets in the start token: gross less gas at base fee plus
/// bid, less the L1 and other costs.
pub fn net_wei(gross: U256, gas: u64, base_fee: u128, bid: u128, other_wei: u128) -> i128 {
    let fees = u128::from(gas).saturating_mul(base_fee.saturating_add(bid)).saturating_add(other_wei);
    (gross.low_u128() as i128).saturating_sub(fees as i128)
}
```

Add `pub mod eval;` to `armed/mod.rs`.

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 --offline -p apex-runtime --test armed_eval`

---

### Task 7: `apex armed-eval`: logs, fire quotes, simulation, verdicts, summary

**Files:**
- Create: `crates/apex-runtime/src/armed/run.rs`
- Modify: `crates/apex-chain/src/rpc/ws.rs` (`parse_log`), `crates/apex-chain/src/base/flashblock_stream.rs` (`FlashblockSeen: Deserialize`), `crates/apex-runtime/src/main.rs` (`armed-eval`)
- Test: `crates/apex-runtime/tests/armed_run.rs`; `crates/apex-chain/tests/ws_feed.rs` (a `parse_log` case)

**Interfaces:**
- Consumes: Tasks 2–6; `PoolBook::load`, `PoolBook::apply_writes`, `frontier::build`, `LiveCycle::quote_fixed`, `gas::MEASURED`.
- Produces:
  - `apex_chain::rpc::ws::parse_log(r: &Value, pending: bool) -> Option<RawLog>`;
  - `armed::run::{effects_of(logs: &[RawLog], venues: &BTreeMap<Address, Venue>) -> Vec<TxEffect>, Verdict, run(config: &ShadowConfig) -> Result<RunReport, String>, summary(dir: &Path) -> Result<Summary, String>}`;
  - `apex armed-eval [--config PATH]` and `apex armed-eval --summary [--dir DIR]`.

- [ ] **Step 1: Write the failing tests.**

`crates/apex-chain/tests/ws_feed.rs`, a new case:

```rust
#[test]
fn a_log_object_from_get_logs_parses_like_a_notification() {
    let r = serde_json::json!({
        "address": "0xd0b53d9277642d899df5c87a3966a349a798f224",
        "topics": ["0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"],
        "data": "0x",
        "blockNumber": "0x10",
        "transactionHash": "0x00000000000000000000000000000000000000000000000000000000000000ab",
        "transactionIndex": "0x3",
        "logIndex": "0x7",
        "removed": false
    });
    let l = apex_chain::rpc::ws::parse_log(&r, false).unwrap();
    assert_eq!((l.block_number, l.transaction_index, l.log_index), (Some(16), Some(3), Some(7)));
    assert!(!l.pending);
}
```

`crates/apex-runtime/tests/armed_run.rs`:

```rust
use alloy_primitives::{address, b256, Address, B256};
use apex_chain::rpc::ws::RawLog;
use apex_runtime::armed::eval::Write;
use apex_runtime::armed::run::effects_of;
use apex_runtime::live::feed::{BURN, MINT, SWAP};
use apex_runtime::live::inventory::Venue;
use std::collections::BTreeMap;

const POOL: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");

fn word(v: i128) -> [u8; 32] {
    let mut w = if v < 0 { [0xff; 32] } else { [0; 32] };
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

fn log(topics: Vec<B256>, data: Vec<[u8; 32]>, tx_index: u64, log_index: u64) -> RawLog {
    RawLog {
        pending: false,
        removed: false,
        address: POOL,
        topics,
        data: data.concat(),
        block_number: Some(100),
        transaction_hash: Some(B256::repeat_byte(tx_index as u8)),
        log_index: Some(log_index),
        transaction_index: Some(tx_index),
    }
}

/// A Swap sets the price word, the liquidity word and the tick; a Mint moves
/// liquidity over its range; one transaction's logs are one effect.
#[test]
fn logs_become_ordered_effects_on_the_watched_words() {
    let venues: BTreeMap<Address, Venue> = [(POOL, Venue::UniswapV3)].into_iter().collect();
    let tick_topic = |t: i32| B256::from(word(i128::from(t)));
    let swap = log(vec![SWAP, B256::ZERO, B256::ZERO], vec![word(-5), word(7), word(1 << 96), word(1_000), word(-12)], 4, 9);
    let mint = log(vec![MINT, B256::ZERO, tick_topic(-60), tick_topic(60)], vec![word(0), word(500), word(1), word(1)], 4, 10);
    let burn = log(vec![BURN, B256::ZERO, tick_topic(-60), tick_topic(60)], vec![word(200), word(1), word(1)], 6, 2);
    let effects = effects_of(&[burn, swap, mint], &venues);
    assert_eq!(effects.len(), 2, "two transactions");
    assert_eq!((effects[0].tx_index, effects[1].tx_index), (4, 6), "chain order");
    let a = format!("{POOL:#x}");
    assert_eq!(effects[0].writes[0], Write::Set { address: a.clone(), slot: 0, value: ethers_core::types::U256::one() << 96 });
    assert_eq!(effects[0].writes[1], Write::Set { address: a.clone(), slot: 4, value: 1_000u64.into() });
    assert_eq!(effects[0].writes[2], Write::Tick { address: a.clone(), tick: -12 });
    assert_eq!(effects[0].writes[3], Write::Liquidity { address: a.clone(), slot: 4, lower: -60, upper: 60, delta: 500 });
    assert_eq!(effects[1].writes[0], Write::Liquidity { address: a, slot: 4, lower: -60, upper: 60, delta: -200 });
    let _ = b256!("0000000000000000000000000000000000000000000000000000000000000000");
}
```

- [ ] **Step 2: Run, expect compile failures:** `cargo test -j 3 --offline -p apex-chain --test ws_feed` and `cargo test -j 3 --offline -p apex-runtime --test armed_run`

- [ ] **Step 3: Implement.**

`crates/apex-chain/src/rpc/ws.rs`: move the `Kind::Logs | Kind::PendingLogs` arm's body into

```rust
/// One log object, as `eth_getLogs` returns it or a `logs` notification
/// carries it. `pending` for `pendingLogs`.
pub fn parse_log(r: &Value, pending: bool) -> Option<RawLog> {
    let topics = r
        .get("topics")?
        .as_array()?
        .iter()
        .map(|t| t.as_str().and_then(|s| s.parse().ok()))
        .collect::<Option<Vec<B256>>>()?;
    Some(RawLog {
        pending,
        removed: r.get("removed").and_then(Value::as_bool).unwrap_or(false),
        address: r.get("address")?.as_str()?.parse().ok()?,
        topics,
        data: alloy_primitives::hex::decode(r.get("data")?.as_str()?).ok()?,
        block_number: r.get("blockNumber").and_then(quantity),
        transaction_hash: r.get("transactionHash").and_then(|v| v.as_str()?.parse().ok()),
        log_index: r.get("logIndex").and_then(quantity),
        transaction_index: r.get("transactionIndex").and_then(quantity),
    })
}
```

(keep the field extraction exactly as the current arm does it; the arm becomes `Kind::Logs | Kind::PendingLogs => parse_log(r, kind == Kind::PendingLogs).map(Notification::Log)`).

`crates/apex-chain/src/base/flashblock_stream.rs`: `FlashblockSeen` derives `serde::Deserialize` too.

`crates/apex-runtime/src/armed/run.rs`. Its parts, in order:

```rust
//! `apex armed-eval` (R26): every closed arm judged against the chain, and a
//! verdict per arm, written to the verdict ledger.
//!
//! One run takes arms whose window closed at least 30 blocks ago and after
//! the cursor (`<verdicts dir>/armed-eval.cursor`). For each it:
//! 1. rebuilds the watched words from one `eth_getLogs` over the run's pools;
//! 2. finds the fire point and the rival (`eval::judge`);
//! 3. quotes the fire exactly: both pools loaded at the fire block's parent,
//!    that block's writes replayed up to the trigger, then `quote_fixed`;
//! 4. checks the quote with `eth_simulateV1`: the fire block's earlier
//!    transactions on those pools, then the armed call from the trader;
//! 5. places both in their flashblocks (the flashblock ledger) and judges
//!    the landing for every bid fraction.

use super::eval::{self, bid_wei_per_gas, judge, landing, merge_renewals, net_wei, Landing, Point, TxEffect, Write, F_PERMILLE};
use super::lines::{Arm, ArmedLine};
use super::predicate::layout;
use crate::live::book::{PoolBook, StateWrite, SwapWrite, SyncWrite};
use crate::live::feed::{decode_swap, decode_sync, BURN, MINT, PANCAKE_SWAP, SWAP, SYNC};
use crate::live::frontier::{self, WETH};
use crate::live::gas;
use crate::live::inventory::{self, PoolSpec, Venue};
use crate::live::reads::ChainReads;
use crate::shadow::config::ShadowConfig;
use crate::shadow::ledger;
use alloy_primitives::{keccak256, Address, B256};
use apex_chain::base::flashblock_stream::FlashblockSeen;
use apex_chain::rpc::failover::{FailoverSettings, FailoverTransport};
use apex_chain::rpc::ws::{parse_log, RawLog};
use apex_chain::rpc::RpcTransport;
use apex_types::ids::ChainId;
use ethers_core::types::U256;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Blocks a window must have closed for before its arm is judged.
const SETTLE_BLOCKS: u64 = 30;
/// Blocks per `eth_getLogs`.
const LOG_CHUNK: u64 = 500;
```

Effects from logs:

```rust
fn word_i32(topic: &B256) -> i32 {
    i32::from_be_bytes(topic.0[28..32].try_into().unwrap_or([0; 4]))
}

fn data_u128(data: &[u8], i: usize) -> Option<u128> {
    let w = data.get(i * 32..i * 32 + 32)?;
    Some(u128::from_be_bytes(w[16..32].try_into().ok()?))
}

/// The watched pools' logs as per-transaction effects, in chain order.
pub fn effects_of(logs: &[RawLog], venues: &BTreeMap<Address, Venue>) -> Vec<TxEffect> {
    let mut by_tx: BTreeMap<(u64, u64), (String, Vec<(u64, Write)>)> = BTreeMap::new();
    for l in logs.iter().filter(|l| !l.removed) {
        let (Some(block), Some(txi), Some(li), Some(venue)) =
            (l.block_number, l.transaction_index, l.log_index, venues.get(&l.address))
        else {
            continue;
        };
        let lay = layout(*venue);
        let a = format!("{:#x}", l.address);
        let mut writes = Vec::new();
        match l.topics.first() {
            Some(t) if *t == SWAP || *t == PANCAKE_SWAP => {
                if let (Some(s), Some(ps), Some(ls)) = (decode_swap(l), lay.price_slot, lay.liquidity_slot) {
                    writes.push(Write::Set { address: a.clone(), slot: ps, value: apex_types::compat::u256_to_ethers(s.sqrt_price_x96) });
                    writes.push(Write::Set { address: a.clone(), slot: ls, value: U256::from(s.liquidity) });
                    writes.push(Write::Tick { address: a.clone(), tick: s.tick });
                }
            }
            Some(t) if *t == SYNC => {
                if let (Some(s), Some((s0, s1))) = (decode_sync(l), lay.reserve_slots) {
                    writes.push(Write::Set { address: a.clone(), slot: s0, value: apex_types::compat::u256_to_ethers(s.reserve0) });
                    writes.push(Write::Set { address: a.clone(), slot: s1, value: apex_types::compat::u256_to_ethers(s.reserve1) });
                }
            }
            // Mint(sender, amount, amount0, amount1) and Burn(amount, amount0, amount1),
            // with the range in topics 2 and 3.
            Some(t) if (*t == MINT || *t == BURN) && l.topics.len() == 4 => {
                let amount = if *t == MINT { data_u128(&l.data, 1) } else { data_u128(&l.data, 0) };
                if let (Some(amount), Some(ls)) = (amount, lay.liquidity_slot) {
                    let delta = i128::try_from(amount).unwrap_or(i128::MAX);
                    writes.push(Write::Liquidity {
                        address: a.clone(),
                        slot: ls,
                        lower: word_i32(&l.topics[2]),
                        upper: word_i32(&l.topics[3]),
                        delta: if *t == MINT { delta } else { -delta },
                    });
                }
            }
            _ => {}
        }
        let tx = l.transaction_hash.map_or_else(String::new, |h| format!("{h:#x}"));
        let entry = by_tx.entry((block, txi)).or_insert_with(|| (tx, Vec::new()));
        entry.1.extend(writes.into_iter().map(|w| (li, w)));
    }
    by_tx
        .into_iter()
        .map(|((block, tx_index), (tx, mut writes))| {
            writes.sort_by_key(|(li, _)| *li);
            TxEffect { block, tx_index, tx, writes: writes.into_iter().map(|(_, w)| w).collect() }
        })
        .collect()
}
```

The run itself. `Verdict` and `RunReport`:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerF {
    pub f_permille: u32,
    pub bid_wei_per_gas: String,
    pub landing: Landing,
    /// When the quote priced the fire: net in wei if landed, optimistic and pessimistic.
    pub net_wei: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Verdict {
    pub kind: String, // "verdict"
    pub at_ns: u64,
    pub arm_id: u64,
    pub route_hash: String,
    pub trigger_leg: u8,
    pub rung: u8,
    pub fired: bool,
    /// A higher rung of the same slot fired on the same transaction: it, not this, would execute.
    pub shadowed: bool,
    pub fire: Option<(u64, u64, String, Option<u32>)>,
    pub rival: Option<(u64, u64, String, Option<u32>, String)>, // ..., priority wei per gas
    /// WETH gross of the fire, and the intermediate residue; `None` with a reason when it would revert.
    pub gross_wei: Option<String>,
    pub residue: Option<String>,
    pub quote_error: Option<String>,
    pub sim: Option<SimCheck>,
    pub base_fee_wei: String,
    pub per_f: Vec<PerF>,
    pub weth_usd: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimCheck {
    pub ok: bool,
    pub gross_wei: Option<String>,
    pub gas_used: u64,
    pub agrees: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct RunReport {
    pub judged: usize,
    pub fired: usize,
    pub quoted: usize,
    pub simulated: usize,
    pub sim_agreed: usize,
    pub cursor: u64,
}
```

`run(config)`:

```rust
pub async fn run(config: &ShadowConfig) -> Result<RunReport, String> {
    let urls: Vec<String> = config.rpc_http.iter().map(|u| u.expose().clone()).collect();
    let rpc: Arc<dyn RpcTransport> =
        Arc::new(FailoverTransport::connect(&urls, config.chain_id, FailoverSettings::default()).await.map_err(|e| e.to_string())?);
    let reads = ChainReads::new(Arc::clone(&rpc));
    let head = reads.head().await.map_err(|e| e.to_string())?;
    let dir = config.armed.verdicts.parent().map(Path::to_path_buf).unwrap_or_default();
    let cursor_path = dir.join("armed-eval.cursor");
    let cursor: u64 = std::fs::read_to_string(&cursor_path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let now = now_ns();

    // Today's and yesterday's arms: a window is ten seconds.
    let lines = read_lines::<ArmedLine>(&config.armed.ledger, now, 2)?;
    let due: Vec<Arm> = merge_renewals(lines)
        .into_iter()
        .filter(|a| a.to_block > cursor && a.to_block + SETTLE_BLOCKS <= head)
        .collect();
    let mut report = RunReport { cursor, ..Default::default() };
    if due.is_empty() {
        return Ok(report);
    }
    let new_cursor = due.iter().map(|a| a.to_block).max().unwrap_or(cursor);
    let specs = inventory_specs(config)?;
    let venues: BTreeMap<Address, Venue> = specs.iter().map(|s| (s.pool, s.venue)).collect();
    let pools: BTreeSet<Address> = due.iter().flat_map(|a| a.pools.iter().filter_map(|p| p.parse().ok())).collect();
    let from = due.iter().map(|a| a.armed_block + 1).min().unwrap_or(head);
    let to = due.iter().map(|a| a.to_block).max().unwrap_or(head);
    let logs = get_logs(&*rpc, &pools, from, to).await?;
    let effects = effects_of(&logs, &venues);
    let flashblocks = read_lines::<FlashblockSeen>(&config.flashblocks.ledger, now, 2).unwrap_or_default();
    let weth_usd = weth_usd(&reads, head).await.unwrap_or(0.0);

    let mut judged: Vec<(Arm, eval::Judged)> = Vec::new();
    for arm in due {
        let own: Vec<TxEffect> = effects
            .iter()
            .filter(|e| e.writes.iter().any(|w| arm.pools.iter().any(|p| match w {
                Write::Set { address, .. } | Write::Tick { address, .. } | Write::Liquidity { address, .. } => address == p,
            })))
            .cloned()
            .collect();
        if let Some(j) = judge(&arm, &own) {
            judged.push((arm, j));
        }
    }
    // A slot's rungs that fire on one transaction: only the highest executes first.
    let mut top: BTreeMap<(String, u8, u64, u64), (u8, u64)> = BTreeMap::new();
    for (a, j) in &judged {
        if let Some(f) = &j.fire {
            let k = (a.route_hash.clone(), a.trigger_leg, f.block, f.tx_index);
            let e = top.entry(k).or_insert((a.rung, a.arm_id));
            if a.rung > e.0 {
                *e = (a.rung, a.arm_id);
            }
        }
    }

    let mut out = Vec::new();
    for (arm, j) in judged {
        report.judged += 1;
        let shadowed = j.fire.as_ref().is_some_and(|f| {
            top.get(&(arm.route_hash.clone(), arm.trigger_leg, f.block, f.tx_index)).is_some_and(|(_, id)| *id != arm.arm_id)
        });
        let v = verdict(&reads, &*rpc, config, &specs, &arm, &j, &logs, &flashblocks, shadowed, weth_usd, now, &mut report).await;
        out.push(v);
    }
    for v in &out {
        ledger::append_line(&config.armed.verdicts, now, v).map_err(|e| e.to_string())?;
    }
    report.cursor = new_cursor;
    std::fs::write(&cursor_path, report.cursor.to_string()).map_err(|e| e.to_string())?;
    Ok(report)
}
```

`verdict(...)`, the per-arm I/O:

```rust
#[allow(clippy::too_many_arguments)]
async fn verdict(
    reads: &ChainReads,
    rpc: &dyn RpcTransport,
    config: &ShadowConfig,
    specs: &[PoolSpec],
    arm: &Arm,
    j: &eval::Judged,
    logs: &[RawLog],
    flashblocks: &[FlashblockSeen],
    shadowed: bool,
    weth_usd: f64,
    now: u64,
    report: &mut RunReport,
) -> Verdict {
    let mut v = Verdict {
        kind: "verdict".into(), at_ns: now, arm_id: arm.arm_id, route_hash: arm.route_hash.clone(),
        trigger_leg: arm.trigger_leg, rung: arm.rung, fired: j.fire.is_some(), shadowed,
        fire: None, rival: None, gross_wei: None, residue: None, quote_error: None, sim: None,
        base_fee_wei: arm.base_fee_wei.clone(), per_f: Vec::new(), weth_usd,
    };
    let Some(fire) = &j.fire else { return v };
    report.fired += 1;
    let block = block_object(rpc, fire.block, true).await;
    let base_fee = block.as_ref().and_then(|b| hex_u128(b.get("baseFeePerGas")?)).unwrap_or(0);
    v.base_fee_wei = base_fee.to_string();

    // 3. The exact quote at the fire.
    match quote_at_fire(reads, specs, arm, fire, logs).await {
        Ok((gross, residue)) => {
            report.quoted += 1;
            v.gross_wei = Some(gross.to_string());
            v.residue = Some(residue.to_string());
        }
        Err(e) => v.quote_error = Some(e),
    }
    // 4. The simulation check.
    if let Some(b) = &block {
        if let Some(sim) = simulate(rpc, config, arm, fire, b, logs).await {
            report.simulated += 1;
            let agrees = v.gross_wei.as_deref() == sim.gross_wei.as_deref();
            report.sim_agreed += usize::from(agrees);
            v.sim = Some(SimCheck { agrees, ..sim });
        }
    }
    // 5. Positions and the rival's bid. Flashblocks when the ledger places
    // both; otherwise whole blocks (index 0), where a rival in the fire's own
    // block competes by fee and the pessimistic case gives it the block.
    let fire_fb = locate(flashblocks, fire.block, fire.tx_index);
    let rival_fb = j.rival.as_ref().map(|r| locate(flashblocks, r.block, r.tx_index));
    let granular = fire_fb.is_some() && rival_fb.map_or(true, |x| x.is_some());
    v.fire = Some((fire.block, fire.tx_index, fire.tx.clone(), fire_fb));
    let fire_at = (fire.block, if granular { fire_fb.unwrap_or(0) } else { 0 });
    let mut rival_at = None;
    let mut rival_bid = 0u128;
    if let Some(r) = &j.rival {
        let r_fb = rival_fb.flatten();
        rival_bid = priority_of(rpc, r).await.unwrap_or(0);
        v.rival = Some((r.block, r.tx_index, r.tx.clone(), r_fb, rival_bid.to_string()));
        rival_at = Some((r.block, if granular { r_fb.unwrap_or(0) } else { 0 }));
    }
    let surplus = U256::from_dec_str(&arm.surplus_wei).unwrap_or_default();
    let gross = v.gross_wei.as_deref().and_then(|g| U256::from_dec_str(g).ok());
    let other: u128 = arm.other_wei.parse().unwrap_or(0);
    for f in F_PERMILLE {
        let bid = bid_wei_per_gas(surplus, arm.gas_expected, f);
        let l = landing(fire_at, rival_at, bid, rival_bid);
        let net = gross.map(|g| net_wei(g, arm.gas_expected, base_fee, bid, other).to_string());
        v.per_f.push(PerF { f_permille: f, bid_wei_per_gas: bid.to_string(), landing: l, net_wei: net });
    }
    v
}
```

`quote_at_fire`:

```rust
async fn quote_at_fire(reads: &ChainReads, specs: &[PoolSpec], arm: &Arm, fire: &Point, logs: &[RawLog]) -> Result<(U256, U256), String> {
    let pools: Vec<Address> = arm.pools.iter().filter_map(|p| p.parse().ok()).collect();
    let mine: Vec<PoolSpec> = specs.iter().filter(|s| pools.contains(&s.pool)).cloned().collect();
    let (book, unloaded) = PoolBook::load(reads, &mine, fire.block - 1).await.map_err(|e| e.to_string())?;
    if !unloaded.is_empty() {
        return Err("a pool did not load at the fire's parent".into());
    }
    let mut writes = Vec::new();
    for l in logs.iter().filter(|l| {
        l.block_number == Some(fire.block) && pools.contains(&l.address) && l.transaction_index.is_some_and(|i| i <= fire.tx_index)
    }) {
        let at = (fire.block, l.log_index.unwrap_or(0));
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
    let snap = book.snapshot();
    let (_, cycles) = frontier::build(ChainId::BASE, WETH, &snap);
    let cycle = cycles.values().find(|c| format!("{:#x}", c.commitment.route_hash) == arm.route_hash).ok_or("the route is not in the book")?;
    let live = crate::live::pricing::LiveCycle::new(cycle, &snap).ok_or("a pool left its ladder")?;
    let size = U256::from_dec_str(&arm.size_wei).map_err(|e| e.to_string())?;
    let mid = U256::from_dec_str(&arm.mid).map_err(|e| e.to_string())?;
    let q = live.quote_fixed(size, mid).ok_or("hop 1 returns less than mid: it would revert")?;
    let gross = q.outputs[1].checked_sub(size + U256::one()).map(|g| g + U256::one()).ok_or("below the loan plus one: it would revert")?;
    Ok((gross, q.outputs[0] - mid))
}
```

(Use the field names `SyncWrite` actually declares in `book.rs`; it is the `Sync` analogue of `SwapWrite`.)

`simulate`: the fire block's transactions with index up to and including the trigger whose hashes appear among the arm's pools' logs in that block, replayed as calls, then the armed call from `config.signer.address`. Use the same call-object field names `live/sim.rs` sends to `eth_simulateV1`:

```rust
async fn simulate(rpc: &dyn RpcTransport, config: &ShadowConfig, arm: &Arm, fire: &Point, block: &Value, logs: &[RawLog]) -> Option<SimCheck> {
    let pools: Vec<Address> = arm.pools.iter().filter_map(|p| p.parse().ok()).collect();
    let touched: BTreeSet<B256> = logs
        .iter()
        .filter(|l| l.block_number == Some(fire.block) && pools.contains(&l.address))
        .filter_map(|l| l.transaction_hash)
        .collect();
    let mut calls: Vec<Value> = block
        .get("transactions")?
        .as_array()?
        .iter()
        .filter(|t| {
            let i = t.get("transactionIndex").and_then(hex_u128).unwrap_or(u128::MAX);
            let h: Option<B256> = t.get("hash").and_then(|h| h.as_str()?.parse().ok());
            i <= u128::from(fire.tx_index) && h.is_some_and(|h| touched.contains(&h))
        })
        .map(|t| json!({"from": t["from"], "to": t["to"], "input": t["input"], "value": t["value"], "gas": t["gas"]}))
        .collect();
    calls.push(json!({
        "from": format!("{:#x}", config.signer.address),
        "to": format!("{:#x}", config.executor.address),
        "input": arm.calldata,
        "gas": format!("{:#x}", arm.gas_limit),
    }));
    let params = json!([{
        "blockStateCalls": [{
            "blockOverrides": {
                "number": format!("{:#x}", fire.block),
                "time": block["timestamp"],
                "baseFeePerGas": block["baseFeePerGas"],
            },
            "calls": calls,
        }],
        "validation": false,
    }, format!("{:#x}", fire.block - 1)]);
    let out = rpc.call("eth_simulateV1", params).await.ok()?;
    let last = out.get(0)?.get("calls")?.as_array()?.last()?.clone();
    let ok = last.get("status").and_then(Value::as_str) == Some("0x1");
    let gas_used = last.get("gasUsed").and_then(hex_u128).unwrap_or(0) as u64;
    let topic = keccak256(b"ProfitRealised(address,uint256,uint256)");
    let gross = last.get("logs")?.as_array()?.iter().find_map(|l| {
        let t0: B256 = l.get("topics")?.get(0)?.as_str()?.parse().ok()?;
        (t0 == topic).then(|| l.get("data")?.as_str().and_then(|d| U256::from_str_radix(&d[2..66], 16).ok()))?
    });
    Some(SimCheck { ok, gross_wei: gross.map(|g| g.to_string()), gas_used, agrees: false })
}
```

The small helpers:

```rust
fn now_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

fn hex_u128(v: &Value) -> Option<u128> {
    u128::from_str_radix(v.as_str()?.trim_start_matches("0x"), 16).ok()
}

/// The JSON lines of a daily ledger for `days` days up to now.
fn read_lines<T: serde::de::DeserializeOwned>(base: &Path, now: u64, days: u64) -> Result<Vec<T>, String> {
    let mut out = Vec::new();
    for d in (0..days).rev() {
        let path: PathBuf = ledger::daily_path(base, now.saturating_sub(d * 86_400_000_000_000));
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        out.extend(text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()));
    }
    Ok(out)
}

fn inventory_specs(config: &ShadowConfig) -> Result<Vec<PoolSpec>, String> {
    inventory::load(&config.inventory, config.universe.filter()).map_err(|e| e.to_string())
}

async fn get_logs(rpc: &dyn RpcTransport, pools: &BTreeSet<Address>, from: u64, to: u64) -> Result<Vec<RawLog>, String> {
    let addresses: Vec<String> = pools.iter().map(|a| format!("{a:#x}")).collect();
    let topics: Vec<String> = [SWAP, PANCAKE_SWAP, SYNC, MINT, BURN].iter().map(|t| format!("{t:#x}")).collect();
    let mut out = Vec::new();
    let mut start = from;
    while start <= to {
        let end = (start + LOG_CHUNK - 1).min(to);
        let v = rpc
            .call("eth_getLogs", json!([{"address": addresses, "topics": [topics], "fromBlock": format!("{start:#x}"), "toBlock": format!("{end:#x}")}]))
            .await
            .map_err(|e| e.to_string())?;
        out.extend(v.as_array().ok_or("eth_getLogs: not an array")?.iter().filter_map(|l| parse_log(l, false)));
        start = end + 1;
    }
    Ok(out)
}

async fn block_object(rpc: &dyn RpcTransport, n: u64, full: bool) -> Option<Value> {
    rpc.call("eth_getBlockByNumber", json!([format!("{n:#x}"), full])).await.ok()
}

/// The rival's effective priority fee per gas.
async fn priority_of(rpc: &dyn RpcTransport, r: &Point) -> Option<u128> {
    let t = rpc.call("eth_getTransactionByHash", json!([r.tx])).await.ok()?;
    let b = block_object(rpc, r.block, false).await?;
    let base = hex_u128(b.get("baseFeePerGas")?)?;
    match (t.get("maxPriorityFeePerGas").and_then(hex_u128), t.get("maxFeePerGas").and_then(hex_u128)) {
        (Some(p), Some(m)) => Some(p.min(m.saturating_sub(base))),
        _ => Some(hex_u128(t.get("gasPrice")?)?.saturating_sub(base)),
    }
}

/// The flashblock that added `tx_index` to `block`, from the ledger.
fn locate(fbs: &[FlashblockSeen], block: u64, tx_index: u64) -> Option<u32> {
    fbs.iter()
        .filter(|f| f.block == block)
        .find(|f| f.first_tx.is_some_and(|first| tx_index >= u64::from(first) && tx_index < u64::from(first) + u64::from(f.tx_count)))
        .map(|f| f.index)
}

/// WETH in USD from the deep Uniswap WETH/USDC pool's price at `head`.
async fn weth_usd(reads: &ChainReads, head: u64) -> Option<f64> {
    let pool: Address = "0xd0b53D9277642d899DF5C87A3966A349A798F224".parse().ok()?;
    let w = reads.storage_at(pool, 0, head).await.ok()?;
    let sqrt = (w & super::predicate::price_mask()).as_u128() as f64 / 2f64.powi(96);
    Some(sqrt * sqrt * 1e12)
}
```

`summary(dir)`: reads every `armed-verdicts-*.jsonl` in `dir`, and per UTC day and per `f`:
- fires (not shadowed);
- optimistic and pessimistic wins;
- net in USD (`net_wei / 1e18 × weth_usd`, summed over wins);
- included reverts (a fire whose `quote_error` says it would revert, counted when it would have landed);
- simulation agreement (agreed over simulated).

It prints one JSON object:

```rust
#[derive(Debug, Default, Serialize)]
pub struct DayF { pub fires: u64, pub wins_optimistic: u64, pub wins_pessimistic: u64, pub net_usd_optimistic: f64, pub net_usd_pessimistic: f64, pub included_reverts: u64 }

#[derive(Debug, Default, Serialize)]
pub struct Summary { pub days: BTreeMap<String, BTreeMap<u32, DayF>>, pub simulated: u64, pub sim_agreed: u64, pub arms_judged: u64 }

pub fn summary(dir: &Path) -> Result<Summary, String> {
    let mut s = Summary::default();
    let mut files: Vec<_> = std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten().map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("armed-verdicts-") && n.ends_with(".jsonl")))
        .collect();
    files.sort();
    for f in files {
        let day = f.file_stem().and_then(|n| n.to_str()).map(|n| n.trim_start_matches("armed-verdicts-").to_string()).unwrap_or_default();
        for line in std::fs::read_to_string(&f).map_err(|e| e.to_string())?.lines() {
            let Ok(v) = serde_json::from_str::<Verdict>(line) else { continue };
            s.arms_judged += 1;
            if let Some(sim) = &v.sim {
                s.simulated += 1;
                s.sim_agreed += u64::from(sim.agrees);
            }
            if !v.fired || v.shadowed {
                continue;
            }
            for p in &v.per_f {
                let d = s.days.entry(day.clone()).or_default().entry(p.f_permille).or_default();
                d.fires += 1;
                let usd = p.net_wei.as_deref().and_then(|n| n.parse::<f64>().ok()).map(|n| n / 1e18 * v.weth_usd);
                if p.landing.optimistic {
                    d.wins_optimistic += 1;
                    d.net_usd_optimistic += usd.unwrap_or(0.0);
                    d.included_reverts += u64::from(v.quote_error.as_deref().is_some_and(|e| e.contains("revert")));
                }
                if p.landing.pessimistic {
                    d.wins_pessimistic += 1;
                    d.net_usd_pessimistic += usd.unwrap_or(0.0);
                }
            }
        }
    }
    Ok(s)
}
```

`crates/apex-runtime/src/main.rs`: dispatch `armed-eval` before `shadow`:

```rust
    if std::env::args().nth(1).as_deref() == Some("armed-eval") {
        return armed_eval().await;
    }
```

```rust
/// `apex armed-eval [--config PATH]`: judge closed arms (R26).
/// `apex armed-eval --summary [--dir DIR]`: the bid curve so far; reads files only.
async fn armed_eval() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(2).collect();
    if args.first().map(String::as_str) == Some("--summary") {
        let dir = args.get(2).filter(|_| args.get(1).map(String::as_str) == Some("--dir")).cloned().unwrap_or_else(|| "var/apex".into());
        return match apex_runtime::armed::run::summary(std::path::Path::new(&dir)) {
            Ok(s) => {
                println!("{}", serde_json::to_string_pretty(&s).unwrap_or_default());
                ExitCode::SUCCESS
            }
            Err(e) => {
                error!(error = %e, "armed-eval summary failed");
                ExitCode::FAILURE
            }
        };
    }
    let config = match args.as_slice() {
        [] => DEFAULT_SHADOW_CONFIG.to_string(),
        [flag, path] if flag == "--config" => path.clone(),
        _ => {
            error!("usage: apex armed-eval [--config PATH] | apex armed-eval --summary [--dir DIR]");
            return ExitCode::FAILURE;
        }
    };
    let env = Env::secrets_only();
    let loaded = match apex_runtime::shadow::config::ShadowConfig::load(&config, &env) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "armed-eval: the config did not load");
            return ExitCode::FAILURE;
        }
    };
    match apex_runtime::armed::run::run(&loaded).await {
        Ok(r) => {
            println!("{}", serde_json::to_string(&r).unwrap_or_default());
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "armed-eval failed");
            ExitCode::FAILURE
        }
    }
}
```

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 --offline -p apex-chain -p apex-runtime`. Then build the binary (`cargo build -j 3 --offline -p apex-runtime --bin apex`) and run `target/debug/apex armed-eval --summary --dir /nonexistent`. It must fail cleanly with an error, not panic.

---

### Task 8: The host: timer, runbook, remote and status

**Files:**
- Create: `ops/host/apex-armed-eval.service`, `ops/host/apex-armed-eval.timer`
- Modify: `ops/host/README.md` (an "Armed-shadow" section), `ops/host/remote.sh` (an `armed` command; the file is new on this branch, from R25's follow-up)

- [ ] **Step 1: The units.** `ops/host/apex-armed-eval.service`:

```ini
# R26: judge closed arms against the chain. Nothing is sent. See ops/host/README.md.
[Unit]
Description=APEX armed-eval (Task 8.5 R26): judge armed-shadow arms; nothing is sent
After=network-online.target apex-shadow.service

[Service]
Type=oneshot
User=apex
Group=apex
WorkingDirectory=/opt/apex
EnvironmentFile=/etc/apex/shadow.env
ExecStart=/opt/apex/bin/apex armed-eval --config ops/shadow.base.yaml
UMask=0027
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictSUIDSGID=true
LockPersonality=true
CapabilityBoundingSet=
ReadWritePaths=/opt/apex/var
TimeoutStartSec=240
```

`ops/host/apex-armed-eval.timer`:

```ini
[Unit]
Description=APEX armed-eval every 5 minutes (Task 8.5 R26)

[Timer]
OnBootSec=5min
OnUnitInactiveSec=5min
AccuracySec=10s

[Install]
WantedBy=timers.target
```

- [ ] **Step 2: `ops/host/remote.sh`**: add to the `case`:

```bash
  armed) cmd='/opt/apex/bin/apex armed-eval --summary --dir /opt/apex/var/apex 2>&1 | tail -80' ;;
```

and to the usage comment: `#   ops/host/remote.sh armed         # armed-shadow's bid curve so far (R26)`.

- [ ] **Step 3: `ops/host/README.md`**: a new section, "9. Armed-shadow (R26)", covering:
  - install:
    `install -m 0644 /opt/apex/ops/host/apex-armed-eval.{service,timer} /etc/systemd/system/ && systemctl daemon-reload && systemctl enable --now apex-armed-eval.timer`;
  - checks:
    - `systemctl list-timers apex-armed-eval.timer`;
    - `journalctl -u apex-armed-eval -n 20`;
    - `ops/host/remote.sh armed`;
  - what the summary's fields mean;
  - gate A's thresholds, copied from the spec.

- [ ] **Step 4: Check:** `systemd-analyze verify ops/host/apex-armed-eval.service ops/host/apex-armed-eval.timer` (expect only the binary's absence), and `bash -n ops/host/remote.sh`.

---

### Task 9: Gates, mutations, PLAN.md, commit, deploy

- [ ] **Step 1: Gates.** `bash $SCRATCH/gates.sh`: everything green except the two known-red scripts. `env_coverage` (inside the workspace tests) must pass with `armed-eval` reading only `APEX_SECRET_*` through `Env`.

- [ ] **Step 2: Mutations.** Restore and `touch` after each; run with `--no-fail-fast`. Each mutant must be killed:
  - `move_to_price`: the downward tick (`tick - 1` → `tick`): `cl_move`;
  - `move_to_price`: the crossing comparison (`target <= *at` → `target < *at`): `cl_move`;
  - `move_reserves`: `10_000 + bps` → `10_000 - bps`: `armed_move`;
  - the live check's mismatch (`!=` → `==`): `armed_move`;
  - `trigger_up` (negated): `armed_threshold`;
  - `rungs`'s bisection (`lo = mid` → `lo = mid - 1`), and the rung offsets (`RUNG_OFFSETS[index - 1]` → `0`): `armed_threshold`;
  - `band`'s hop-2-trigger `mid` (`a.min(b).min(..)` → `rung.point.mid`): `armed_threshold`, `armed_engine`;
  - `MoveStats`'s prior (`step <= largest` → `true`): `armed_engine`;
  - the live check's split (tick mismatch counted as a liquidity change): `armed_move`;
  - the engine's freshness filter (`k.seqs != [a.seq, b.seq]` → `false`): `armed_engine`;
  - `judge`'s `trigger_met` (`|=` → `=`): `armed_eval`;
  - the summary's split (`Some(false)` ↔ `Some(true)`) and its shadowed skip: `armed_run`;
  - `band`'s fixed-size check (`>` → `>=` with `margin + 1`): `armed_threshold`;
  - `predicate::trigger`'s op (`AtLeast` ↔ `AtMost`): `armed_predicate`;
  - Pancake's liquidity slot (5 → 4): `armed_predicate`;
  - `armed_call`'s hop 1 floor (`mid` → `1`) and hop 2 floor (`size + 1` → `size`): `armed_predicate`;
  - `Words::apply`'s range test (`t < upper` → `t <= upper`): `armed_eval`;
  - `judge`'s window (`<= arm.to_block` → `<`): `armed_eval`;
  - `landing`'s pessimistic rule (`r < next` → `r <= next`): `armed_eval`;
  - `next_flashblock`'s wrap (`(block + 1, 1)` → `(block + 1, 0)`): `armed_eval`;
  - `bid_wei_per_gas`'s divisor: `armed_eval`;
  - `effects_of`'s Burn sign: `armed_run`;
  - the engine's window end (`*busy <= head`, `to_block <= head`, `to_block > head`, each moved by one), and the changed-rung guard deleted: `armed_engine`.

  A survivor is either tested or deleted.

- [ ] **Step 3: PLAN.md** gets an R26 entry: what was built, the two spec corrections, the mutants, and the deploy.

- [ ] **Step 4: Commit** by named paths:
  - every file this plan created or modified;
  - `ops/host/remote.sh`;
  - `Cargo.lock` if it changed;
  - `PLAN.md`.

  Then `git diff --stat -- <paths>` must be empty. Verify in a clean worktree with its own `CARGO_TARGET_DIR`: `cargo test -j 3 -p apex-math -p apex-chain -p apex-runtime`.

- [ ] **Step 5: Deploy to the host,** with the operator's go-ahead, because it restarts R25's 14-day clock (spec, Rollout 2):
  1. Merge the branch and push.
  2. On the host, through Run Command:
     - `git fetch` in `/home/apex/apex` and check out the new commit;
     - build the release binary in `/opt/apex` the way `ops/host/README.md` §7 does;
     - `install` the binary and `REVISION`;
     - install the two armed-eval units and enable the timer;
     - `systemctl restart apex-shadow`.
  3. Checks:
     - the boot log names any unarmable pools (expect none);
     - after 5 minutes, `ops/host/remote.sh` shows the `armed` and `moves` lines, with arms growing and move mismatches under 1% of checks;
     - after 15 minutes, `ops/host/remote.sh armed` shows judged arms.
  4. Tell the operator the new 14-day clock's start.

- [ ] **Step 6: After 3 days:** read the bid curve with `ops/host/remote.sh armed` and `analyze`, and take gate A to the operator.
