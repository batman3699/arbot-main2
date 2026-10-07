# Aerodrome's second Slipstream factory (Task 8.5 R22) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The shadow prices pools from Aerodrome's second Slipstream factory (`0xf8f2…`), routing their hops through a new executor adapter 3 at that factory's router.

**Architecture:** A new `Venue::SlipstreamV3` reuses Slipstream's pricing, fee model, swap topic and gas model, and differs only in its inventory directory, factory, venue id and adapter. Every venue-keyed rule that is "dynamic fee or not" reads `Venue::fee_is_static`, so there is one source of truth. The adapter registration is an owner transaction the operator broadcasts; until then the shadow leaves the venue out by itself.

**Tech Stack:** Rust, Python (fee script and chain checks), `cast` (owner transaction), BlockPI RPC.

Spec: `docs/superpowers/specs/2026-10-08-second-slipstream-venue-design.md`. Ships with R21 (`docs/superpowers/plans/2026-10-08-spike-first-capture.md`), whose tasks come first.

## Global Constraints

- Factory `0xf8f2eB4940CFE7d13603DDDD87f123820Fc061Ef`; router `0x698Cb2b6dd822994581fEa6eA4Fc755d1363A92F`; selector `0xa026383e`; adapter id 3; venue id `VenueId(9)`; inventory directory `aerodrome_slipstream_v3`.
- Shared quoter `0xCd2A7D98e82D6107eac1828ce8DeAA6acB65b555`, selector `0x9e7defe6`, tick-spacing field OR'd with `0x080000`.
- Executor clone `0x8940B565D050974b2b589B70De43bCb753b1F93B`; BatchRouter `0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2`; router owner `0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3`; trader `0xCB436Ba3acb945b3fc8EE6345857262584356595`.
- Never load or print `.env`'s `PRIVATE_KEY` (the owner's key) or any secret. The operator broadcasts the owner transaction.
- Never `cargo fmt`; `-j 3` for cargo; stage named paths only; commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- RPC for scripts: `BASE_RPC_URL` (or `PROBE_HTTP`) set from `.env`'s `BLOCKPI_KEY` inside the command, never echoed: `BASE_RPC_URL="https://base.blockpi.network/v1/rpc/$(grep ^BLOCKPI_KEY= .env | cut -d= -f2-)"`.

---

### Task 1: Measure the second factory's fees

**Files:**
- Modify: `scripts/data/measure_slipstream_fees.py`
- Data (outside git): `data/base/aerodrome_slipstream_v3/pools.jsonl`

- [ ] **Step 1: Add `--dir`.** Replace the module-level `INVENTORY` with a root and a flag:

```python
BASE_DATA = REPO / "data" / "base"
```

in `main()`:

```python
    ap.add_argument("--dir", default="aerodrome_slipstream",
                    help="inventory directory under data/base/ (default: aerodrome_slipstream)")
    args = ap.parse_args()
    inventory = BASE_DATA / args.dir / "pools.jsonl"
```

and use `inventory` wherever `INVENTORY` was read, backed up or written. In the docstring's usage line add `[--dir aerodrome_slipstream_v3]`, and one sentence: "Aerodrome's second Slipstream factory (0xf8f2…) has its own directory and the same rule (R22)."

- [ ] **Step 2: Dry-run both directories**

Run: `BASE_RPC_URL="https://base.blockpi.network/v1/rpc/$(grep ^BLOCKPI_KEY= .env | cut -d= -f2-)" python3 scripts/data/measure_slipstream_fees.py --dry-run` then the same with `--dir aerodrome_slipstream_v3 --dry-run`.
Expected: the first measures 0 (R20 already did); the second about 35, none unreadable.

- [ ] **Step 3: Measure for real** with `--dir aerodrome_slipstream_v3`.
Expected: "35 measured at block N", and a backup named `pools.jsonl.bak.fees-<ts>` in that directory. A second run measures 0.

---

### Task 2: The venue

**Files:**
- Modify: `crates/apex-venues/src/adapter.rs` (`venue_ids`)
- Modify: `crates/apex-runtime/src/live/inventory.rs` (`Venue`, `load`)
- Modify: `crates/apex-runtime/src/live/frontier.rs` (adapter constants)
- Modify: `crates/apex-runtime/src/live/calls.rs` (`step`, `binding`)
- Modify: `crates/apex-runtime/src/live/feed.rs` (`swap_topic`)
- Modify: `crates/apex-runtime/src/live/book.rs` (TWAP refresh filter ~line 539)
- Modify: `crates/apex-runtime/src/live/admission.rs` (`fee_behavior`)
- Modify: `crates/apex-runtime/src/live/gas.rs` (`PerVenue::of`)
- Test: `crates/apex-runtime/tests/live_calls.rs`, `crates/apex-runtime/tests/live_venues.rs` (new)

**Interfaces:**
- Produces: `Venue::SlipstreamV3`; `venue_ids::AERODROME_SLIPSTREAM_V3 = VenueId(9)`; `frontier::SLIPSTREAM_V3_ADAPTER: u16 = 3`; `frontier::SLIPSTREAM_V3_ROUTER: Address`.

- [ ] **Step 1: Write the failing tests.** New file `crates/apex-runtime/tests/live_venues.rs`:

```rust
//! R22: Aerodrome's second Slipstream factory is a venue of its own.

use alloy_primitives::address;
use apex_runtime::live::feed::swap_topic;
use apex_runtime::live::gas::{self, HopSteps};
use apex_runtime::live::inventory::Venue;
use apex_venues::adapter::venue_ids;

#[test]
fn the_second_slipstream_factory_is_its_own_venue() {
    let v = Venue::SlipstreamV3;
    assert!(Venue::ALL.contains(&v));
    assert_eq!(v.dir(), "aerodrome_slipstream_v3");
    assert_eq!(v.factory(), address!("f8f2eB4940CFE7d13603DDDD87f123820Fc061Ef"));
    assert_eq!(v.id(), venue_ids::AERODROME_SLIPSTREAM_V3);
    assert!(!v.fee_is_static(), "its fee module is dynamic, as the first factory's is");
}

#[test]
fn its_pools_emit_the_first_factorys_swap_topic() {
    assert_eq!(swap_topic(Venue::SlipstreamV3), swap_topic(Venue::Slipstream));
}

/// The same router and pool code as the first factory, so the same gas.
#[test]
fn it_settles_at_the_first_factorys_gas() {
    let hop = |venue| HopSteps { venue, zero_for_one: true, crossed: 3, word_steps: 1 };
    let uni = HopSteps { venue: Venue::UniswapV3, zero_for_one: false, crossed: 1, word_steps: 0 };
    assert_eq!(
        gas::MEASURED.estimate(&[hop(Venue::SlipstreamV3), uni]),
        gas::MEASURED.estimate(&[hop(Venue::Slipstream), uni]),
    );
}
```

In `live_calls.rs`: import `SLIPSTREAM_V3_ADAPTER, SLIPSTREAM_V3_ROUTER` from `apex_runtime::live::frontier`; add an arm to `expected_step`:

```rust
        Venue::SlipstreamV3 => {
            let call = slipstream_exact_input_single(&SlipstreamSwap {
                token_in,
                token_out,
                tick_spacing: p.state.tick_spacing,
                recipient: EXECUTOR,
                deadline: DEADLINE,
                amount_in,
                min_out,
            })
            .unwrap();
            (Op::Generic, generic_step(SLIPSTREAM_V3_ADAPTER, token_in, amount_in, &call))
        }
```

and append:

```rust
/// The second factory's $3.8M WETH/USDC pool (tick spacing 50), from
/// `data/base/aerodrome_slipstream_v3/pools.jsonl`.
const SLIP3: Address = address!("3FE04A59Ebd38cF06080a6F60a98D124eb59392A");

/// R22: a hop in the second Slipstream factory's pool is adapter 3's router
/// call, the same `exactInputSingle` as the first factory's.
#[test]
fn a_second_slipstream_hop_is_adapter_threes_router_call() {
    for (uni, slip) in [(-197_350, -197_300), (-197_300, -197_350)] {
        let b = Arc::new(PoolBook::from_snapshots(
            [pool(UNI, Venue::UniswapV3, uni, 500, 10), pool(SLIP3, Venue::SlipstreamV3, slip, 400, 100)],
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
    }
}

#[test]
fn the_second_slipstream_factory_binds_adapter_three() {
    let b = binding(Venue::SlipstreamV3).expect("an adapter venue");
    assert_eq!((b.id, b.router), (SLIPSTREAM_V3_ADAPTER, SLIPSTREAM_V3_ROUTER));
    assert_eq!(b.id, 3);
    assert_eq!(b.selector, binding(Venue::Slipstream).unwrap().selector, "the same exactInputSingle");
}

#[tokio::test]
async fn the_second_slipstream_factory_is_reachable_once_adapter_three_is_registered() {
    assert!(!reach(deployed(true)).await.contains(&Venue::SlipstreamV3), "not before the owner registers it");
    let r = deployed(true);
    let v3 = binding(Venue::SlipstreamV3).unwrap();
    r.holds(v3.id, SLIPSTREAM_V3_ROUTER, v3.selector, true);
    assert_eq!(reach(r).await, vec![Venue::UniswapV3, Venue::Slipstream, Venue::PancakeV3, Venue::SlipstreamV3]);
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -j 3 -p apex-runtime --test live_venues --test live_calls`
Expected: compile errors, no variant `SlipstreamV3`.

- [ ] **Step 3: Implement.**

`apex-venues/src/adapter.rs`, in `venue_ids`:

```rust
    /// Aerodrome's second Slipstream factory, `0xf8f2…` (R22).
    pub const AERODROME_SLIPSTREAM_V3: VenueId = VenueId(9);
```

`inventory.rs`: add the variant last (so `Venue::ALL`'s order, which `reachable_venues` keeps, only grows at the end):

```rust
pub enum Venue {
    UniswapV3,
    Slipstream,
    PancakeV3,
    /// Aerodrome's second Slipstream deployment, factory `0xf8f2…`: the same
    /// pool and router code as the first, its own pools, router and fee
    /// module (R22). Named after its inventory directory.
    SlipstreamV3,
}
```

`ALL: [Self; 4] = [Self::UniswapV3, Self::Slipstream, Self::PancakeV3, Self::SlipstreamV3]`; `id` → `venue_ids::AERODROME_SLIPSTREAM_V3`; `dir` → `"aerodrome_slipstream_v3"`; `factory` → `address!("f8f2eB4940CFE7d13603DDDD87f123820Fc061Ef")`, with a comment that its router's `factory()` returns it (read 2026-10-08). `fee_is_static` needs no change. In `load`, make the dynamic-fee rule one rule:

```rust
            let fee = match (r.fee_ppm_onchain, venue) {
                (Some(f), _) => f,
                // A dynamic-fee record's `fee` is its tick spacing: skipped,
                // never guessed.
                (None, v) if !v.fee_is_static() => continue,
                (None, _) => match r.fee {
                    Some(f) => f,
                    None => continue,
                },
            };
```

`frontier.rs`, after `PANCAKE_SMART_ROUTER`:

```rust
/// The adapter id Aerodrome's second Slipstream factory's router is registered
/// under: by the owner's `registerAdapter`, after the deploy (R22).
pub const SLIPSTREAM_V3_ADAPTER: u16 = 3;

/// The second Slipstream factory's `SwapRouter`: what adapter 3 must be. Its
/// `factory()` is `0xf8f2…`, its code (9,908 bytes, as the first router's) holds
/// `exactInputSingle` `0xa026383e` (read 2026-10-08).
pub const SLIPSTREAM_V3_ROUTER: Address = address!("698Cb2b6dd822994581fEa6eA4Fc755d1363A92F");
```

`calls.rs` `step`: the Slipstream arm serves both, taking its adapter from `binding`:

```rust
            Venue::Slipstream | Venue::SlipstreamV3 => {
                let call = slipstream_exact_input_single(&SlipstreamSwap { /* unchanged */ })
                    .map_err(|e| refuse(e.to_string()))?;
                // Each factory's pools run through its own router's adapter.
                let adapter = binding(leg.venue).ok_or_else(|| refuse("no adapter for a Slipstream hop".to_string()))?.id;
                Ok(Step { op: Op::Generic, data: generic_step(adapter, leg.token_in, amount_in, &call) })
            }
```

(keep the struct literal's fields as they are; check `refuse`'s parameter type and pass a `String` or `&str` to match). In `binding`:

```rust
        Venue::SlipstreamV3 => Some(AdapterBinding {
            id: SLIPSTREAM_V3_ADAPTER,
            router: SLIPSTREAM_V3_ROUTER,
            selector: SLIPSTREAM_EXACT_INPUT_SINGLE,
        }),
```

`feed.rs` `swap_topic`: `Venue::UniswapV3 | Venue::Slipstream | Venue::SlipstreamV3 => SWAP,`.

`book.rs` ~539: `.filter(|(s, _)| !s.venue.fee_is_static())` (with a comment: every dynamic-fee venue follows its TWAP).

`admission.rs`:

```rust
        fee_behavior: Some(if p.spec.venue.fee_is_static() {
            FeeBehavior::Static { ppm: p.state.fee_ppm }
        } else {
            FeeBehavior::Dynamic
        }),
```

`gas.rs` `PerVenue::of`: `Venue::Slipstream | Venue::SlipstreamV3 => self.slipstream,` with a comment: the second factory's pools and router are the first's code, so its crossings cost the same (checked by R22's router simulation).

Any other non-exhaustive `match` the compiler reports gets the arm its Slipstream neighbour has.

- [ ] **Step 4: Run the tests**

Run: `cargo test -j 3 -p apex-runtime --test live_venues --test live_calls --test live_book --test live_gas --test live_feed --test shadow_parts && cargo test -j 3 -p apex-venues`
Expected: all pass. If a `shadow_parts` test counts the shipped universe from `data/`, its count grows with the new pools: update the expected figure and say why in its comment.

---

### Task 3: Prove the book prices the new pools exactly

**Files:**
- Create (scratchpad, not committed): `<scratchpad>/r22_parity/Cargo.toml`, `<scratchpad>/r22_parity/src/main.rs`

**Interfaces:**
- Consumes: `PoolBook::load(&ChainReads, &[PoolSpec], u64) -> Result<(PoolBook, Vec<Unloaded>), ReadError>`; `ChainReads::new(Arc<dyn RpcTransport>)`; `FailoverTransport::connect(&[String], ChainId, FailoverSettings)`; `apex_runtime::shadow::universe(&Path, &[Venue], UniverseFilter)`; `quote_exact_input_multi_tick(&ClPoolState, &TickLadder, U256, bool, u32)`; `MAX_TICKS`.

- [ ] **Step 1: Write the probe.** `Cargo.toml`:

```toml
[package]
name = "r22_parity"
version = "0.1.0"
edition = "2021"

[dependencies]
apex-runtime = { path = "/home/scotty/arbot-main2/arbot-main-main/crates/apex-runtime" }
apex-chain = { path = "/home/scotty/arbot-main2/arbot-main-main/crates/apex-chain" }
apex-math = { path = "/home/scotty/arbot-main2/arbot-main-main/crates/apex-math" }
apex-types = { path = "/home/scotty/arbot-main2/arbot-main-main/crates/apex-types" }
ethers-core = "2.0.14"
alloy-primitives = "*"
serde_json = "1"
tokio = { version = "1", features = ["full"] }
```

`src/main.rs`, with one routine per pool `spec` from `universe(Path::new("data/base"), &Venue::ALL, UniverseFilter { max_fee_ppm: 3_000, min_depth_usd: 100_000.0 })` whose `venue == Venue::SlipstreamV3` (adjust `min_depth_usd`'s literal to the field's type):
  1. `eth_getLogs` over the last 3,000 blocks for the pool's Swap topic `0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67`; take the newest three distinct blocks.
  2. **Fee check**, at each of the three blocks: `PoolBook::load(&reads, &[spec.clone()], block)`, then compare the loaded pool's `state.fee_ppm` with `fee()` (`0xddca3f43`) read by `eth_call` at that block. Print both.
  3. **Quote parity** at the newest block: for token0→token1 and token1→token0, at 0.01, 0.1 and 1 WETH (for the non-WETH side, the amount that 0.01, 0.1 and 1 WETH quote to through the book), compute `quote_exact_input_multi_tick(&pool.state, &pool.ladder, amount, zero_for_one, MAX_TICKS)` and `eth_call` the shared quoter `0xCd2A7D98e82D6107eac1828ce8DeAA6acB65b555` with `0x9e7defe6` ++ `abi(tokenIn, tokenOut, amountIn, tickSpacing | 0x080000, 0)` (five words; the first word of the answer is `amountOut`). Print pool, direction, size, book, quoter, the difference in units and bps, and ticks crossed.

  RPC from `PROBE_HTTP` (environment), set inside the run command as under Global Constraints; never print it.

- [ ] **Step 2: Build and run it**

Run: `cd <scratchpad>/r22_parity && CARGO_TARGET_DIR=<scratchpad>/probe-target cargo build -j 3 --release`, then from the repository root `PROBE_HTTP="https://base.blockpi.network/v1/rpc/$(grep ^BLOCKPI_KEY= .env | cut -d= -f2-)" <scratchpad>/probe-target/release/r22_parity`.
Expected: every fee equal at every block; every quote equal to the wei, or within a few units on a multi-tick quote. A fee mismatch or a larger quote gap is a defect: stop and debug it before Task 6, starting from the memory note "Quote exactness is step for step".

---

### Task 4: Prove a swap through the router executes, at Slipstream's gas

**Files:**
- Create (scratchpad, not committed): `<scratchpad>/r22_router_sim.py`

- [ ] **Step 1: Write the script.** Using `eth_simulateV1` at `latest` (the request shape Tier 2 uses), with a state override that gives a fresh address `0x00000000000000000000000000000000000a11ce` 1 WETH and the router an allowance:
  - WETH (`0x4200…0006`) is WETH9: `balanceOf` at slot 3, `allowance` at slot 4. The balance slot is `keccak256(pad32(holder) ++ pad32(3))`; the allowance slot is `keccak256(pad32(router) ++ keccak256(pad32(holder) ++ pad32(4)))`. Use `cast keccak` or `eth_hash`/`pycryptodome` if installed (`python3 -c "import Crypto"`); otherwise compute the two slots with `cast index` (`cast index address <holder> 3`, then `cast index address <router> <that slot as the mapping's slot>`).
  - Two calls in one simulation: `exactInputSingle` (`0xa026383e`) of 0.1 WETH for USDC on the new factory's router `0x698Cb2b6dd822994581fEa6eA4Fc755d1363A92F` with tick spacing 50 (its pool `0x3FE04A59Ebd38cF06080a6F60a98D124eb59392A`), and the same on the first factory's router `0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5` with tick spacing 100 (its pool `0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59`). Confirm each pool's `tickSpacing()` (`0xd0c93a7c`) on-chain before the call. Arguments: `(tokenIn, tokenOut, int24 tickSpacing, recipient, uint256 deadline (now + 600), uint256 amountIn, uint256 amountOutMinimum 0, uint160 sqrtPriceLimitX96 0)`.
  - Print each call's status, `gasUsed` and decoded `amountOut`.

- [ ] **Step 2: Run it** with `BASE_RPC_URL` set inside the command.
Expected: both succeed; their gas differs only by the ticks each crosses (Slipstream's model: `gas::MEASURED` per crossing). A failure or a gap the crossings do not explain is a defect to debug before Task 6.

---

### Task 5: Prepare the owner transaction

**Files:**
- Create (scratchpad, not committed): `<scratchpad>/r22_register.sh`

- [ ] **Step 1: Check the router allows the clone as a target**

Run: `cast call 0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2 "isAllowedTarget(address)(bool)" 0x8940B565D050974b2b589B70De43bCb753b1F93B --rpc-url "$URL"` with `URL` set inside the command.
Expected: `true` (adapter 2's registration went through it).

- [ ] **Step 2: Build the calldata**

```bash
REG=$(cast calldata "registerAdapter(uint16,address)" 3 0x698Cb2b6dd822994581fEa6eA4Fc755d1363A92F)
ALLOW=$(cast calldata "allowSelector(uint16,bytes4)" 3 0xa026383e)
CALL=$(cast calldata "multicall(address[],bytes[])" "[0x8940B565D050974b2b589B70De43bCb753b1F93B,0x8940B565D050974b2b589B70De43bCb753b1F93B]" "[$REG,$ALLOW]")
echo "$CALL"
```

- [ ] **Step 3: Dry-run as the owner and as the trader**

Run: `cast call --from 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3 0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2 "$CALL" --rpc-url "$URL"` and `cast estimate` with the same arguments; then the `cast call` again `--from 0xCB436Ba3acb945b3fc8EE6345857262584356595`.
Expected: as the owner it succeeds (gas about 90,000, as adapter 2's was); as the trader it reverts.

- [ ] **Step 4: Hand the operator the broadcast.** Give the calldata and the command, which prompts for the owner's key rather than taking it in the command line:

```bash
cast send 0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2 "<CALL>" --rpc-url "https://base.blockpi.network/v1/rpc/<your key>" --interactive
```

and how to confirm it afterwards: `adapterOf(3)` returns the router and `isSelectorAllowed(3, 0xa026383e)` returns true, read with `cast call` on the executor clone.

---

### Task 6: Mutation checks, gates, commit

**Files:**
- Modify: `PLAN.md` (R22 entry after R21)

- [ ] **Step 1: Mutation checks** (each alone, `--no-fail-fast`, restore and `touch` after):
  1. `binding`: `SlipstreamV3`'s `id: SLIPSTREAM_V3_ADAPTER` → `SLIPSTREAM_ADAPTER` → `the_second_slipstream_factory_binds_adapter_three` fails.
  2. `calls.rs` `step`: `binding(leg.venue)…id` → `SLIPSTREAM_ADAPTER` → `a_second_slipstream_hop_is_adapter_threes_router_call` fails.
  3. `Venue::factory` for `SlipstreamV3` → the first factory's address → `the_second_slipstream_factory_is_its_own_venue` fails.
  4. `PerVenue::of`: `SlipstreamV3` → `self.uniswap_v3` → `it_settles_at_the_first_factorys_gas` fails.
  5. `swap_topic`: `SlipstreamV3` → `PANCAKE_SWAP` → `its_pools_emit_the_first_factorys_swap_topic` fails.

- [ ] **Step 2: PLAN.md R22 entry** after R21's: the evidence, the verified addresses, the fees measured (count and block), Task 3's parity and fee results, Task 4's gas, the owner transaction prepared (not yet broadcast), the mutant count.

- [ ] **Step 3: Run the gates** with `<scratchpad>/gates.sh` (R21 Task 6 recreated it). Expected: only `check_placeholder_endpoints.sh` and `cl_parity_sweep.sh` FAIL; clippy exit 0; no test failures; forge passes.

- [ ] **Step 4: Commit** by named paths: `crates/apex-venues/src/adapter.rs crates/apex-runtime/src/live/inventory.rs crates/apex-runtime/src/live/frontier.rs crates/apex-runtime/src/live/calls.rs crates/apex-runtime/src/live/feed.rs crates/apex-runtime/src/live/book.rs crates/apex-runtime/src/live/admission.rs crates/apex-runtime/src/live/gas.rs crates/apex-runtime/tests/live_calls.rs crates/apex-runtime/tests/live_venues.rs scripts/data/measure_slipstream_fees.py PLAN.md`, plus any test file Task 2 Step 4 updated. Message: `feat(venues): Aerodrome's second Slipstream factory, through adapter 3 (Task 8.5 R22)`. Then `git diff --stat -- <paths>` must print nothing.

- [ ] **Step 5: Verify the commit** in a clean worktree with its own target: `CARGO_TARGET_DIR=<scratchpad>/wt-target cargo test -j 3 --locked -p apex-venues -p apex-search -p apex-runtime --all-targets`. Remove the worktree after.

---

### Task 7: Roll out R21 and R22

**Files:**
- Memory: `shadow-run-operations.md`, `spike-backlog-serial-plane.md`, `base-mev-market-size.md`, `MEMORY.md`

- [ ] **Step 1: Wait for the operator's broadcast.** Then confirm: `cast call 0x8940B565D050974b2b589B70De43bCb753b1F93B "adapterOf(uint16)(address)" 3` returns `0x698Cb2b6…`, and `"isSelectorAllowed(uint16,bytes4)(bool)" 3 0xa026383e` returns `true`. If the operator wants R21 live before broadcasting, restart without it: R22 stays dormant until a later restart.

- [ ] **Step 2: Release build:** `CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo build -j 3 --release -p apex-runtime --bin apex`.

- [ ] **Step 3: Restart.** SIGINT the running shadow by its pid (`var/apex/shadow.pid`) and wait with `timeout 90 tail --pid=<pid> -f /dev/null`. Archive the log, report and misses to `var/apex/postdeploy-15/` (the segment from 2026-10-07 04:39 UTC). Start with `setsid nohup scripts/shadow.sh > var/apex/shadow.log 2>&1 < /dev/null &`.

- [ ] **Step 4: Check the boot.** The boot line lists `SlipstreamV3` among the venues (once registered), about 26 pools held and the route count the filter gives; within ten minutes `shadow-status.sh` shows the `queue` line and `unpriced 0`, no read failures, and a block named for Tier 2.

- [ ] **Step 5: Tell the operator** where the 14-day clock now counts from, and arm the stop-watch again.

- [ ] **Step 6: Memory.** `shadow-run-operations.md`: the new run's start, commit and universe. `spike-backlog-serial-plane.md`: the fix is built (commit). `base-mev-market-size.md`: the second Slipstream factory is priced. `MEMORY.md` lines to match.
