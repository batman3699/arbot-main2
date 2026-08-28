# Arbot handoff — 2026-08-22

Zero shadow executions to date. This is the operating manual + the state of the
diagnosis. Everything below was measured, not inferred, unless marked HYPOTHESIS.

---

## 1. Run a shadow run (safe — never broadcasts)

```bash
cd /home/scotty/arbot-main2/arbot-main-main
pkill -x arb-exec 2>/dev/null
SP=/tmp/arbot-scratch; mkdir -p $SP
wc -l < logs/candidates-base.jsonl > $SP/pre.txt
rm -f $SP/live_cd.jsonl logs/shadow.jsonl
set -a; . ./.env; set +a
export SHADOW_MODE=true ARBOT_DUMP_CALLDATA=1 \
  ARBOT_DUMP_CALLDATA_PATH=$SP/live_cd.jsonl RUST_LOG=arb_exec=info
setsid ./target/release/arb-exec > $SP/run.log 2>&1 < /dev/null &
disown
```

`SHADOW_MODE=true` is the only thing preventing live broadcast. Never flip it
until a shadow run actually dispatches and you have reviewed the plans.

**Always stop it when done — it burns RPC continuously:**

```bash
pkill -x arb-exec
```

## 2. Read the funnel

```bash
cd /home/scotty/arbot-main2/arbot-main-main
python3 - <<'PY'
import json,collections
PRE=int(open('/tmp/arbot-scratch/pre.txt').read().strip())
rows=[]
with open('logs/candidates-base.jsonl') as fh:
    for i,l in enumerate(fh):
        if i<PRE: continue
        try: rows.append(json.loads(l))
        except: pass
print(f"{len(rows)} new records")
for k,v in collections.Counter(r.get('stage') for r in rows).most_common(): print(f"  {k:32s} {v}")
for k,v in collections.Counter(r.get('rejection_reason') for r in rows if r.get('rejection_reason')).most_common(8): print(f"   {v:5d} {k}")
PY
wc -l logs/shadow.jsonl 2>/dev/null || echo "shadow.jsonl absent = ZERO dispatches"
```

Only one number matters: lines in `logs/shadow.jsonl`. Reduced rejection counts
are not progress.

## 3. Replay a failing plan to get the REAL revert reason

The candidate log records only `failed` with no reason. This is how every real
diagnosis in this project was actually obtained — do this first, always.

```bash
cd /home/scotty/arbot-main2/arbot-main-main
RPC=$(grep -oP '^BASE_RPC_URLS=\K[^,]*' .env)
python3 - "$RPC" <<'PY'
import json,sys,urllib.request,time
rpc=sys.argv[1]
recs=[json.loads(l) for l in open('/tmp/arbot-scratch/live_cd.jsonl') if l.strip()]
def call(r,gas):
    p={"jsonrpc":"2.0","id":1,"method":"eth_call","params":[
        {"to":r['to'],"from":r['from'],"gas":hex(gas),"data":r['data']},hex(r['block'])]}
    q=urllib.request.Request(rpc,json.dumps(p).encode(),
        {'content-type':'application/json','user-agent':'curl/8.5.0','accept':'*/*'})
    d=json.loads(urllib.request.urlopen(q,timeout=60).read())
    if 'error' in d: return d['error'].get('message','')[:70]
    v=d.get('result','0x')
    return f"SUCCESS grossProfit={int(v,16)/1e18:+.9f} WETH" if v!='0x' else "SUCCESS"
for r in recs[:10]:
    print(f"  {call(r,4_000_000)}")
    time.sleep(0.3)
PY
```

`startV2` returns `lastGrossProfit`, so a SUCCESS line prints the actual profit.

Full EVM trace of one plan (BlockPI supports `debug_traceCall`):

```bash
curl -s -X POST "$RPC" -H 'content-type: application/json' --data \
 '{"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[
   {"to":"0xdbfb219b4f1ce08fa61c5cd3c08c1307760caec6","from":"<signer>",
    "gas":"0x3d0900","data":"<calldata>"},"<blockhex>",{"tracer":"callTracer"}]}'
```

## 4. Check the edge universe (the current blocker)

A pair with only ONE pool cannot be arbitraged. Count pairs reachable by >=2
pools **across all venues** — the per-venue count is misleading:

```bash
cd /home/scotty/arbot-main2/arbot-main-main
python3 - <<'PY'
import json,glob,collections
pair=collections.defaultdict(set)
tot=0
for f in glob.glob('data/base/*/pools.jsonl'):
    v=f.split('/')[2]
    for l in open(f):
        try: p=json.loads(l)
        except: continue
        t0,t1=p.get('token0'),p.get('token1')
        if not t0 or not t1: continue
        tot+=1
        pair[tuple(sorted([t0.lower(),t1.lower()]))].add((v,p.get('pool')))
multi={k:v for k,v in pair.items() if len(v)>1}
print(f"pools={tot}  pairs={len(pair)}  pairs with >=2 pools = {len(multi)}")
xv=sum(1 for v in multi.values() if len({a for a,_ in v})>1)
print(f"  of those, CROSS-VENUE capable = {xv}")
for f in sorted(glob.glob('data/base/*/pools.jsonl')):
    print(f"  {f.split('/')[2]:24s} {sum(1 for _ in open(f))}")
PY
```

## 5. Verify a pool is not one-sided (the defect that cost weeks)

`liquidity()` can be huge while one side holds nothing. Always check BOTH
balances, normalised by decimals.

```bash
P=<pool>; RPC=$(grep -oP '^BASE_RPC_URLS=\K[^,]*' .env)
cast call $P 'liquidity()(uint128)' --rpc-url $RPC
T0=$(cast call $P 'token0()(address)' --rpc-url $RPC); T1=$(cast call $P 'token1()(address)' --rpc-url $RPC)
cast call $T0 'balanceOf(address)(uint256)' $P --rpc-url $RPC; cast call $T0 'decimals()(uint8)' --rpc-url $RPC
cast call $T1 'balanceOf(address)(uint256)' $P --rpc-url $RPC; cast call $T1 'decimals()(uint8)' --rpc-url $RPC
```

Reference failure: `0xdea629c5587037d0925ff85f1961d95db62bedd6` reports
`liquidity()=2.446e22` while holding **248.9 WETH against 0.154 bsdETH**. The
local sim quoted 2 WETH -> 1.904 bsdETH; the chain pays 0.0000169. A ~112,000x
overstatement that made it the top-ranked edge on every block for weeks.

---

## What is PROVEN correct (stop re-investigating these)

- Executor contract, module dispatch, Balancer callback, plan encoding — all
  correct. Traces reach `pool.swap()` on both hops.
- Deployed impl bytecode matches source (24,403 B; differs only in immutables).
- `Op` / `LoanProvider` enum ordinals match Rust <-> Solidity exactly.
- Generic step encoding `(target, call, action, token, amount)` matches.
- Access control passes (`executors[signer]=true`).
- Wallet balance is NOT the cause of reverts — identical revert with no gasPrice,
  0.01 gwei, and the 5000 gwei cap. Executor holding 0 ETH/WETH is by design;
  the flash loan funds it in-transaction.

## Defects found and fixed (all uncommitted, ~462 lines / 6 files)

1. **OOG masked as a plan error.** `pool.swap()` ran out of gas; OOG returns
   empty revert data; `_executeSteps:708` turns that into `InvalidGenericAction()`
   (`0xf19db938`). Same calldata: 450k/3M -> `0xf19db938`, 4M+ -> real router
   error. `max_gas_units_per_tx` 500k -> 4,000,000.
2. **Phantom edges.** Exhausted multi-tick quotes fell back to
   `quote_exact_input_single_tick`, which holds liquidity constant (assumes
   infinite depth). `hop_expected_out` now returns `Option`; refusal kills the
   cycle. Added `MultiTickQuote::is_unfillable`.
3. **`ARBOT_LOCAL_CL_QUOTES=0`** — price CL edges via on-chain QuoterV2. This is
   what actually killed the bsdETH phantom (4/4 plans -> 0). The local sim
   trusts `liquidity()` and cannot be fixed by tick data alone.
4. **250x hole in the size grid.** Was `[base/1000, base/4, base/2, base, 2b,
   4b, 8b, 10b]` — nothing between 0.002 and 0.5 WETH, exactly the profitable
   band. Now a dense geometric (2x) ladder.
5. **`MAX_FLASH_LOAN_WEI=2.0` clamped `base` for every token** — which is why
   every plan in the entire project was exactly 2.0 WETH.
6. `ARBOT_CL_LADDER_WORDS=8` (2 was too narrow; exhaustion then meant "we
   couldn't see", not "pool can't fill" — my first fix starved the edge set to
   zero candidates until this was raised).

## Economics — the constraint that bounds everything

To clear `min_net_profit_usd: 2` at ~10bps fees + ~$0.15 gas:

| size | spread needed |
|------|---------------|
| 0.02 WETH | 308 bps (impossible) |
| 0.5 WETH  | 22 bps |
| 1.0 WETH  | 16 bps |
| 2.0 WETH  | 13 bps |

Viable band is **0.5–2.0 WETH**, and only in pools deep enough that the trade's
own price impact stays under the spread. 2 WETH in the 10,046-WETH USDC pool is
0.02% of depth — negligible. 2 WETH in a 10-WETH pool is not.

## Current tunables

```
ARBOT_LOCAL_CL_QUOTES=0     ARBOT_CL_MULTI_TICK=1     ARBOT_CL_LADDER_WORDS=8
ARBOT_DEPTH_DIVISOR=20      MIN_FLASH_LOAN_WEI=0.02   MAX_FLASH_LOAN_WEI=2.0
max_gas_units_per_tx=4000000 (ops/inputs.yaml, base)
ARBOT_MAX_CYCLE_FEE_BPS default 60   min_net_profit_usd: 2
```

## Next actions, in order

1. **Re-run §4 now that the inventory is expanded.** Target metric: pairs with
   >=2 pools, and specifically CROSS-VENUE capable. Every candidate this project
   ever produced was `venue_path: ['univ3','slipstream']`, and
   `aerodrome_slipstream/pools.jsonl` had only **24 pools** while
   `pancakeswap_v3` had **20**. If those are now populated, this is the change
   most likely to produce a first execution.
2. **Wire the revert string into the candidate record.** `simulation_status`
   currently stores only `failed`. Every diagnosis in this project required
   manual calldata replay because of that. Highest-leverage debugging fix.
3. **Apply `is_unfillable` on the single-tick path too.** It currently only
   guards multi-tick quotes, so pools without a tick ladder bypass it entirely
   — that is why fix #2 alone did not stop the phantom.
4. **`quote_block_lag` and `path_tokens` are never populated** — dead fields.
   Populate them; without `quote_block_lag` you cannot tell whether staleness is
   costing you fills.
5. HYPOTHESIS, untested: scan-to-scan p50 was ~8.4s against 2s Base blocks. If
   quotes are several blocks stale at simulation time, opportunities are gone
   before dispatch. Measure with `quote_block_lag` before optimising for it.

## A1 / A2 — stop starving the pipeline (owner's plan, 2026-08-26)

### A1. Flash capacity observability + correctness

Known anchors in the tree:
- `flash_loan_quotes` in `src/main.rs` (~4643). This is where a single
  `capped_amount` was previously applied to ALL providers regardless of what
  each could actually lend; `FlashCapacityCache` / `capacity_capped_amount`
  were added this session and **fail closed** — a provider whose capacity
  cannot be read lends nothing. That fail-closed behaviour is correct but it is
  SILENT, which is exactly the starvation being chased. Log it.
- Balancer V2 vault on Base: `0xBA12222222228d8Ba445958a75a0704d566BF2C8`
  (0 bps). Aave V3 is 5 bps (`FLASHLOAN_PREMIUM_TOTAL()`); prefer Balancer.
- WETH `0x4200000000000000000000000000000000000006`,
  USDC `0x833589fCD6eDb6E08f4c7C32D4f71b54bDa02913`.

Do:
1. Emit per-block at `info`: `provider, token, balance, min_loan, ok=balance>=min`.
   A capacity refresh that fails must log at `warn` — today a failed read is
   indistinguishable from "no opportunity".
2. Verify vault balances directly before trusting the code path:
   `cast call <token> 'balanceOf(address)(uint256)' <vault> --rpc-url $RPC`.
   For Aave the lendable amount is the **aToken reserve**, not the pool address
   balance — resolve via `getReserveData(asset)` and read the aToken. Getting
   this wrong reads as zero capacity and silently disables Aave.
3. Gate cycle starts on `allowlisted AND capacity>=min AND native price
   reliable`. Order WETH first, then USDC. Defer cbBTC/cbETH until both green.

### A2. Native price

Known anchors:
- `native_price_for(token, native_prices)` in `src/main.rs`; the strict
  conversion helper is `tokens_for_native_strict`.
- Rejection `unreliable_native_price_for_start_token` fires around
  `src/main.rs:5644` — it aborts the candidate BEFORE sizing, so an unpriceable
  token silently removes every cycle starting at it. This was already observed
  in the candidate log early in this project.

Do:
1. Counters per token: `priced / no_route / unknown`. Without these you cannot
   distinguish "no arb" from "we refused to price the start token".
2. Fallback chain: UniV3 quoter -> Slipstream quoter -> deepest Solidly /
   Aerodrome WETH pair -> fail closed. Keep the WETH identity path.
3. **Never cache `Unknown` as unpriceable.** A transient RPC failure that gets
   memoised turns into a permanent, invisible loss of every cycle through that
   token. Cache successes; retry unknowns. If a negative cache is kept at all it
   needs a short TTL and a log line on every hit.

### On latency (be careful here)

Measured earlier this session: univ3 collection p50 ~3,660 ms, scan-to-scan p50
~8.4 s, against 2 s Base blocks. That is 4+ blocks stale, not flashblock range.

Two separate things must not be conflated:
- **`src/convex.rs` (Angeris/Chitra/Evans/Boyd)** improves *sizing quality* —
  marginal-rate equalisation across parallel venues and a cardinality prune for
  gas. It does not make the scanner faster. It is also still UNCOMMITTED and not
  wired into the scan loop.
- **Scan latency** is a pipeline/architecture problem (batching, subscription
  vs polling, incremental state). Nothing in the convex work addresses it.

Before optimising latency, populate `quote_block_lag` (currently never written)
and measure how stale quotes actually are at simulation time. If lag is ~0 the
latency theory is dead and the problem is elsewhere; if it is 4+ blocks, that is
a quantified target. Do not rebuild for speed on an unmeasured hypothesis —
that mistake has already cost this project weeks.


## Session 2026-08-26 — measured results

### Fixed, with before/after numbers

1. **Flash-loan bounds unit bug (the #1 starvation).** `flash_loan_quotes` compared
   `capital.{min,max}_flash_loan` (NATIVE, 18dp) against `max_cycle_input` (TOKEN
   raw units). For USDC: `.max(1e17)` forced a request of 100 BILLION USDC, then
   `capacity_capped_amount(available=5.59e10, requested=1e17, min=1e17)` returned
   None. Every USDC-start cycle got zero providers despite 55,886 USDC in the
   Balancer vault. Bounds are now converted to token units at the call site and
   fail closed when unpriceable.
   **`no_flashloan_provider`: 2,286 of 6,000 records -> 0. Throughput ~7x
   (490 records/7.3min vs ~59-71/8min).** Pinned by
   `native_denominated_min_starves_a_six_decimal_token`.

2. **UniV3 `FEE_TIERS` was missing the 100 (1bp) tier.** 63 pools in the Base
   inventory sit there (stable/LST-correlated). Appended LAST, deliberately: the
   native-price loop returns the FIRST non-zero quote, so leading with 1bp would
   let a thin pool outrank a deep 500 pool.

3. **Native price had no Slipstream fallback.** Aerodrome is the dominant CL venue
   on Base and is keyed by tick spacing, not fee tier, so a token priced only on
   Aerodrome resolved `NoRoute`, got cached, and every cycle starting there died
   at `unreliable_native_price_for_start_token` BEFORE sizing. Added
   `SLIPSTREAM_PRICE_TICK_SPACINGS = [1, 50, 100, 200, 2000]`.

4. **Capacity observability (A1).** Success path now logs at INFO with a
   per-provider fundable verdict; aToken resolution failure warns instead of
   silently `continue`ing; `capacity_capped_amount` logs both of its silent
   `None` branches.

### Verified against chain (block ~50461601)

| provider | WETH | USDC |
|---|---|---|
| Balancer vault | 24.52 | 55,886 |
| Aave aToken reserve (CORRECT source) | 7,000.94 | 30,469,107 |
| Aave pool-address balance (WRONG source) | 0.0002 | 114.40 |

The aToken resolver (`resolve_aave_atoken`, selector 0x35ea6a75, word index 8) is
CORRECT and its runtime values match chain exactly. Do not "fix" it.
Note Balancer holds only 24.5 WETH; Aave is the deep WETH provider at 7,000 —
but costs 5bps vs Balancer's 0.

### CURRENT TOP BLOCKER — capacity projection underflow

`no_liquidity` is now the largest rejection (401/490) and it is a MISNOMER. It
fires on WETH, USDC, cbBTC and cbETH — the deepest pairs on the chain.

`graph::cycle_input_capacity` computes
`capacity = min(mul_div_floor(probe, edge.max_input, amount))`, where `amount`
compounds through per-hop rates. Across decimal gaps `amount` grows until
`probe * max_input < amount` and the floor divide yields 0. Instrumented and
measured: **18/18 zero-capacity events were `capacity floored to zero across
hops` at hops=4 and hops=5. Zero dead edges. Zero amount-underflows.**
These cycles have real liquidity; we computed zero and called it absence.

Fix direction (NOT yet implemented): do the projection in higher precision
(U512 intermediate) or reformulate so the divide happens last, and/or normalise
per-hop amounts by token decimals before folding. Validate with a unit test
built from a real 4-hop WETH->USDC->cbBTC->WETH cycle.

Cheap interim probe: cap `max_hops` at 3 and see whether the rejection clears —
if 2-3 hop cycles survive, this is confirmed as a long-cycle arithmetic issue.

Also observed: `probe = 20 WETH` in those log lines, which is 10x
`MAX_FLASH_LOAN_WEI=2.0`. The max clamp is not binding on the probe path —
worth tracing separately.


### U512 capacity fix — DONE, and what it revealed

`cycle_input_capacity` now projects per-hop capacity as an EXACT U512 fraction
(`cycle_input_capacity_exact`). Note `mul_div_floor` was ALREADY U512 internally,
so widening it was never the fix — the sequential FOLD was, because it re-floored
`amount` at every hop. Algebraically the probe cancels:

    capacity_i = max_input_i * PROD(rate_den_j) / PROD(rate_num_j)   for j < i

so the running rate is carried as an exact num/den pair and divided ONCE per hop.
Falls back to the old sequential form only on U512 overflow. Pinned by
`graph::tests::cycle_capacity_survives_a_cross_decimal_multi_hop`.

Result: the arithmetic underflow is gone, but `no_liquidity` did NOT clear
(297/411). Instrumented at the rejection site, and the surviving cause is real:

    hop0 USDC->X   max_in=102.6 USDC, rate => 102.6 USDC buys 9,546 X
    hop1 X->WETH   max_in=0.2087 X    <- binding

Hop 0 pushes 9,546 X into a hop that accepts 0.2087. Projected back that is
~0.0022 USDC of cycle capacity, flooring below one raw unit by hop 4.

### NEXT ROOT CAUSE (not yet fixed): max_input is a probe artifact

`venues::edge_capacity_from_quote` derives capacity as
`probe * tolerance_bps / slippage_bps`, capped at
`probe * EDGE_CAPACITY_PROBE_MULTIPLIER`. So `edge.max_input` is bounded by the
SIZE WE HAPPENED TO QUOTE AT, not by pool depth. An edge quoted with a small
probe reports a small capacity forever, and any cycle wanting to push more is
rejected as `no_liquidity`. A 0.2-token cap on a live WETH pair is not a
liquidity fact about the chain — it is a fact about our probe.

Fix direction: derive `max_input` from measured pool depth (both-sided balance,
already validated elsewhere in this file) rather than from the probe, or re-quote
the edge at the size the cycle actually wants before rejecting it.

### Flash-loan allowlists — VERIFIED, no YAML change needed

`aave_v3` and `balancer_v2` both list all four (WETH/USDC/cbBTC/cbETH) under BOTH
`max_loan_assets` and `allowlist_tokens`. `hub_tokens` carries all four;
`token_seeds` carries none of them (hub covers the universe side).

CAUTION: a separate `balancer_vault` entry exists with EMPTY lists alongside the
populated `balancer_v2`. If the loader keys on `balancer_vault`, the populated
entry is not the one being read. Confirm which name wins before trusting it.

### Test targets — IMPORTANT

`main.rs` compiles into the BIN target. `cargo test --lib` (302) does NOT run it.
Use `cargo test --release --bin arb-exec` (462). Both are green as of this entry.


### max_input from real pool depth — ATTEMPTED, REVERTED, and why

Added and TESTED (kept in tree, currently unwired):
  * `venues::cl_virtual_reserves(state)` — CL depth from pool state.
    reserve0 = L * 2^96 / sqrt_price_x96 ; reserve1 = L * sqrt_price_x96 / 2^96
    (constant-product equivalent at the current price; under-states depth when
    neighbouring ticks hold liquidity, which is the safe direction).
  * `venues::edge_capacity_from_cl_state(state, zero_for_one)` — applies the same
    `EDGE_CAPACITY_RESERVE_BPS` fraction UniV2/Solidly already use.
  * Test `cl_capacity_comes_from_pool_state_not_the_probe` (passes).

Wiring it into the two CL edge construction sites took candidates from ~400 per
7min to ZERO. Reverting restored them (175 in 4min). Causality confirmed by
A/B, not inferred.

ROOT CAUSE OF THAT REGRESSION — `max_input` is doing DOUBLE DUTY:
`edge_health_score_bps` (venues.rs:245) uses `ln(max_input)` as `liquidity_score`,
which is 50% of the composite edge health score. Health gates
`min_edge_health_score_bps` AND ranks edges into `max_edges_hot` (1000). Probe-
derived values sit ~1e17; depth-derived sit ~1e21+, which saturates
`liquidity_score` at its 10,000 cap for EVERY CL edge. The ranking signal
collapses, edge selection goes arbitrary, and cycles stop forming.

To land this properly, SEPARATE THE TWO CONCERNS FIRST:
  1. Give `Edge` a distinct depth field (e.g. `depth_hint`) for ranking, or
     normalise `liquidity_score` per-venue so it is scale-invariant.
  2. Only then set `max_input` from pool depth.
Doing (2) without (1) will reproduce the zero-candidate regression.

NOTE the probe-derived form is still WRONG as a capacity (it is a fact about our
sampling, not the pool) — it is merely load-bearing for ranking today.


### Depth-based max_input — status after 2026-08-27

WHAT IS PROVEN (direct chain measurement, independent of any run):
The virtual-reserve formula `L/sqrt(P)`, `L*sqrt(P)` is NOT a depth measure. It
describes the constant-product curve a CL pool is tangent to at the current
price; that curve runs 0..infinity, well outside the ticks holding liquidity.
Measured against `balanceOf` at latest block:

  WETH/USDC 0x6c561b446  virtual r0 562,148 WETH  vs  10,046 held   (56x over)
                         virtual r1 1.40B USDC    vs  88.3M held    (16x over)
  WETH/bsdETH 0xdea629c5 virtual r1 7.98e21 raw   vs  0.154 bsdETH  (absurd)

So `cl_virtual_reserves` must never set `max_input`. Renamed the public-facing
entry `cl_virtual_reserves_unsafe_for_depth` and pinned it with
`virtual_reserves_overstate_real_depth_and_are_not_a_capacity_source`.

WHAT IS **NOT** PROVEN — read this before trusting the earlier entry:
The claim that wiring depth capacity "took candidates from ~400/7min to ZERO,
confirmed by A/B revert" is CONFOUNDED and should not be relied on. A later run
on the REVERTED code also produced zero records over 4 minutes, and its log
carries a 429 plus 6 timeouts. RPC rate-limiting (from many back-to-back runs)
dominates candidate throughput and invalidates single-run A/B comparisons.

  runG (wired)    0 records / 7min
  runH (reverted) 175 records / 4min
  runI (reverted) 0 records / 4min   <- same code as runH, opposite result

BEFORE any further A/B on this pipeline: pace the runs, check the log for 429 /
timeout, and require at least two consistent runs per arm. Several conclusions in
this project have come from single runs; this is the first measured evidence that
that method is unreliable here.

Also DISREGARD the earlier claim that `edge_health_score_bps` ranking caused the
regression. `liquidity_score = (ln(max_input+1) * 1200).min(10_000)` saturates at
10,000 for any input above ~4,150, so probe-scale (1e17) and depth-scale (1e21)
produce the IDENTICAL score. It cannot have changed ranking.

CORRECT PATH (unchanged, and now with the helper in place):
`edge_capacity_from_pool_balance(balance_in)` is written and tested. To wire it,
thread the pool's token addresses into `cl_sim::load_cl_pool_states_batched` and
fetch `balanceOf` for both tokens alongside slot0/liquidity/tickSpacing/fee
(6 sub-calls per pool instead of 4), then store them on `ClPoolState`.
Real balances are ground truth and cannot overstate.


## 2026-08-27 — balance-derived max_input LANDED

### RETRACTIONS (both earlier claims were wrong; do not re-use them)

1. "The regression came from `ln(max_input)` saturating `edge_health_score_bps`."
   FALSE, arithmetically. `liquidity_score = (ln(max_input+1) * 1200).min(10_000)`
   saturates at 10,000 for ANY input above ~4,150, so probe-scale (~1e17) and
   depth-scale (~1e21) give the IDENTICAL score. Ranking separation was never
   required and was not done.

2. "A/B confirmed the depth wiring zeroed candidates." CONFOUNDED. A later run on
   fully reverted code also produced zero records, under 429s and timeouts.
   Single-run comparisons on this RPC are unreliable.

### CAPACITY SOURCE OF TRUTH

For CL edges it is the pool's token `balanceOf`, NOT `L/sqrt(P)`, `L*sqrt(P)`.
The latter are the VIRTUAL constant-product reserves of the tangent curve, which
runs 0..infinity outside the active ticks. Measured at latest block:

  pool              token   balanceOf        max_input(33.33%)   virtual (WRONG)
  WETH/USDC         WETH       13,010.37           4,336.35        563,701.86  (43x)
  WETH/USDC         USDC   83,263,328.34      27,751,667.34      1.40e9        (17x)
  WETH/cbBTC        WETH        1,773.49             591.10         57,692.63  (33x)
  WETH/cbBTC        cbBTC          99.96              33.32          1,826.44  (18x)
  WETH/bsdETH       bsdETH          0.1540             0.0513        23,941.89  (~466,000x)

### IMPLEMENTED

* `ClPoolState` gained `balance0/balance1: Option<U256>` (derives `Default`).
* `cl_sim::load_cl_pool_states_batched` now takes
  `(pool, fee_hint, token0, token1)` and issues SIX sub-calls per pool in one
  aggregate3 (slot0, liquidity, tickSpacing, fee, balanceOf x2), chunked at 24
  pools (144 sub-calls, comparable to the previous 32x4=128). All reads pinned to
  the same block.
* Per-pool fallback path and backrun post-state carry `None`/observed balances —
  FAIL CLOSED, never invent depth.
* Both UniV3 and Slipstream construction sites: balance-derived capacity when the
  input-side balance is present and non-zero, else the existing probe path.
  `zero_for_one` (`token_in == pool.token0`) selects token0 vs token1.
* Probe path retained for Balancer / Curve / UniV4 and any failed balance read.

### TWO PACED RUNS (both healthy, criteria met)

  run1  257 records   429=11  timeouts=12
  run2  209 records   429= 1  timeouts=12
  reasons (run1/run2): no_liquidity 141/103, plan_build_failed 81/80,
                       no_profitable_size 35/26

Throughput non-zero and comparable across arms; `no_liquidity` did NOT spike —
it fell versus the 297-401 seen before this change. 465 bin / 305 lib tests pass,
panics gate clean.

### GUARDRAIL TEST

`virtual_reserves_are_not_referenced_by_capacity_paths` scans this module's own
source and fails if `cl_virtual_reserves` ever appears on a line that also sets
`max_input`. Needles are assembled with `concat!` so the test does not match
itself through `include_str!`.

### STILL OPEN — this did NOT fix fundability (Phase A)

`no_flashloan_provider` and `unreliable_native_price_for_start_token` are
separate. Return to: flash capacity + native price + restricting cycle starts to
WETH/USDC.


## Phase A status — 2026-08-27 (measured, not assumed)

### BOTH PHASE A GATES ARE NOW GREEN

Over 470 recent records:
  no_flashloan_provider                    2,286 -> 0
  unreliable_native_price_for_start_token    862 -> 0
  pricing_reliable                                 True on 470/470

Fixed by (a) converting flash-loan bounds to token units before comparison,
(b) the Slipstream native-price fallback, (c) adding the 100 (1bp) fee tier.
Phase A fundability is no longer the blocker.

### DONE
* A1 capacity observability: success path logs at INFO with a per-provider
  fundable verdict; aToken resolution failure warns; both silent `None` branches
  of `capacity_capped_amount` log.
* A1 balance verification: Balancer vault + Aave aToken confirmed against chain.
  `resolve_aave_atoken` (0x35ea6a75, word 8) is CORRECT — do not "fix" it.
* A2 per-token metrics (`record_native_price_probe`) already existed.
* A2 `Unknown` is never cached as unpriceable — already correct.
* A2 Slipstream fallback across tick spacings [1,50,100,200,2000].

### IMPLEMENTED BUT DISABLED — start-token restriction

`ARBOT_START_TOKENS` (comma-separated) restricts cycle STARTS. Gate lives in
`flash_loan_quotes` (covers hub selection, rotation and the main path) plus a
capacity/ordering filter in `flash_loan_hub_tokens` (wrapped-native first).
Tests: `start_token_allowlist_parses_or_fails_open`, `start_token_gate_is_unit_free`.

ENABLING IT BREAKS CYCLE FORMATION. Clean A/B, healthy RPC in BOTH arms:
    ON  (WETH,USDC): 0 records   429=1
    OFF            : 154 records 429=5

Applying it only to `flash_loan_hub_tokens` was not enough — cbBTC returned as a
start via ROTATION (74 of 254 records). Moving the gate into `flash_loan_quotes`
then zeroed everything, which points at the `unfundable_anchors` / rotation pass
around main.rs:6700 treating "this anchor has no quotes" as "drop the cycle"
rather than "try the next rotation". Fix that pass before re-enabling; the env
var is commented out in .env and the code is inert while unset.

Motivation stands: cbBTC starts were 126 of 470 records (27%) and 49 of 62
`no_profitable_size`, with zero dispatches.

### NOT DONE
* A2 deepest Solidly/Aerodrome WETH-pair fallback (UniV3 -> Slipstream is in;
  the Solidly leg is not).

### CURRENT TOP REJECTIONS (post-Phase-A)
  no_liquidity        123-247   (WETH/USDC/cbBTC)
  plan_build_failed    80-161   (USDC-heavy; this is the unpriceable-hop guard)
  no_profitable_size    26-62   (cbBTC-heavy)


## plan_build_failed AUDIT — 2026-08-27

FINDING: 100% of `plan_build_failed` (26/26 sampled) was ONE cause — the
unpriceable-hop guard added in `plan.rs::hop_expected_out`:
    "hop ADDR->ADDR is unpriceable: pool cannot fill N"
No other error path contributed. The message was already logged at `warn!`;
only the aggregate reason reached the candidate record.

IT WAS OVER-FIRING. Verified against chain:
    641,933 PLAY rejected; pool 0xf1cacd7e00 (fee 100) holds 6,115,660  -> 0.1x
     48,615 AERO rejected; pool 0xe5b5f522e9 (fee 500) holds   244,323  -> 0.2x
10-20% of a pool is a large trade, not an unfillable one.

ROOT CAUSE: `cl_swap` sets `exhausted` for TWO different situations — liquidity
ran out, and our tick ladder did not reach far enough. The guard treated both as
"pool cannot fill". With `ARBOT_CL_LADDER_WORDS` unset (default 2) almost any
sizeable swap walks off the ladder.

COUNTERINTUITIVE RESULT WORTH REMEMBERING: widening the ladder 2 -> 8 made it
WORSE, 34% -> 47.8% of rejections. A wider ladder means MORE hops reach the
multi-tick path at all, so more of them can report exhausted. The ladder width
was never the bug; the guard was.

FIX: disambiguate exhaustion against the pool's REAL input-side balance — the
same ground truth that now sets `max_input` (added in the balance-threaded
loader). If the size is within the tradeable fraction of actual holdings, the
LADDER was the limit and the hop falls through to the single-tick estimate
(which carries `cl_tick_buffer_bps`). If the balance is unknown, FAIL CLOSED.

    plan_build_failed: 47.8% -> 32.4%   (8-word ladder, balance disambiguation)

Tests: `cl_hop_out_accepts_an_exhausted_ladder_when_real_depth_covers_the_size`
covers all three arms (unknown balance -> reject, ample balance -> price,
size at 100% of holdings -> reject). The original
`cl_hop_out_rejects_an_exhausted_ladder_instead_of_falling_back` still passes;
its fixture has no balances, so it exercises the fail-closed path.

ALSO NOTE: `.env` had lost `ARBOT_CL_LADDER_WORDS=8` and `ARBOT_LOCAL_CL_QUOTES`
was back to 1. Both were set in an earlier session. Re-check these before
attributing behaviour to code.


## Executor authorization — RESOLVED, do not re-investigate

Chain state (Base, clone 0xdbfb219b4f1ce08fa61c5cd3c08c1307760caec6):
    executors[0xd7a4d612..]  hot wallet   -> TRUE
    executors[0x8e04a6ae..]  BatchRouter  -> TRUE
    owner()                               -> 0x8e04a6ae.. (BatchRouter)

The deployment artifacts are misleading: `initialise` sets
`executors[_owner] = true` with `_owner` = BatchRouter, and NO
`setExecutor(hotWallet, true)` transaction is recorded. Nevertheless the hot
wallet IS authorized on chain, so an unrecorded privileged transaction was made.
Trust the chain read, not the artifacts.

Corroboration: `NotExecutor()` = 0xc32d1d76 has NEVER appeared in any candidate
log or run log in this project (0 occurrences). If authorization were blocking,
every startV2 simulation would return exactly that selector. And traces reach
`pool.swap()`, which is only possible past `onlyExecutor`.

=> Direct `executor.startV2(...)` is the correct path. Do NOT route live plans
through BatchRouter and do NOT add authorization automation to fix a blocker
that does not exist.

OPERATIONAL GAP (real, but separate): the authorizing transaction is not in the
deployment records, so a redeploy or a new chain WILL land an executor whose hot
wallet is unauthorized, and the failure mode is every plan reverting 0xc32d1d76.
`launch_chain_runtime` already calls `ensure_executor_allowlisted`, which is the
right guard — it would catch this at startup rather than in production. Add the
setExecutor step to the deployment runbook.


## no_liquidity AUDIT — 2026-08-27

FINDING: `no_liquidity` was NOT a capacity problem. 37/37 sampled were
`projected_capacity_zero` (never `base_amount_zero`), concentrated at 3-5 hops,
and the real cause was MISPRICED EDGES blowing up the projection.

Worked example from the instrumented rejection:
    hop0 WETH->SAPIEN  max_in=1.002e18  rate=3.12e4
    hop1 SAPIEN->USDC  max_in=1.63e24   rate=5.37e9
    hop2 USDC->MAMO    max_in=7.25e10   -> projects to 4.3e-4 -> 0
Pushing 1.002 WETH through hop0+hop1 yields 1.68e32 raw USDC (1.68e26 USDC).

Verified against chain (QuoterV2, both tokens 18dp):
    planner edge:  0.01 SAPIEN -> 5.372e25 raw USDC
    chain fee=10000: 0.01 SAPIEN ->        327 raw USDC
    => overstated ~1.6e23x (twenty-three orders of magnitude)

`rate_num = quote.amount_out`, `rate_den = quote.amount_in`, so the edge's own
quote is the garbage. Such an edge does not just misprice itself — it inflates
`amount` through `cycle_input_capacity` until the projection floors to zero, and
surfaces as `no_liquidity` on cycles that are perfectly sound.

FIX: cap every CL grid quote by the pool's REAL balance of the OUTPUT token
(`cl_grid_quote`, using the balances added to `ClPoolState`). A pool cannot pay
out more than it holds — no model, nothing to argue with, and it catches errors
of any magnitude. Balance unknown => NO cap (fail open); the capacity path fails
closed separately.

TWO RUNS (both heavily rate-limited, 429=359 / 418, but consistent):
    quotes discarded (out > pool balance)   9,657 / 10,372
    no_liquidity          ~48%  ->  5.5% / 3.0%
    plan_build_failed              90%  / 93%
    one candidate reached `below_min_profit_threshold` — the furthest any
    candidate has reached in this project.

INTERPRETATION — READ THIS BEFORE THE NEXT STEP:
~10,000 quotes PER RUN claim more output than the pool holds. That is not a tail
case, it is most of the quote set. Removing them removes most edges, which is why
`no_liquidity` collapsed into `plan_build_failed` rather than into dispatches.
The local CL simulator is producing bad quotes AT SCALE.

`ARBOT_LOCAL_CL_QUOTES=0` routes CL pricing through the on-chain QuoterV2 and
previously eliminated the bsdETH phantom outright (4/4 plans -> 0). It is
currently back to 1. Testing 0 with the output cap in place is the obvious next
experiment and is a one-line change. Cost is RPC per edge — measure against the
latency budget.


## On-chain quoter run — 2026-08-27 (cleanest signal yet)

Config: ARBOT_LOCAL_CL_QUOTES=0, MAX_FLASH_LOAN_WEI=500 WETH,
MIN_FLASH_LOAN_WEI=0.1 WETH, output-balance cap on quotes, balance-derived
max_input. RPC HEALTHY (429=1, timeouts=12) so this is a clean read.

    10 records in 7.3 min
    100% no_profitable_size
    ZERO plan_build_failed  (was 90-93% with the local sim)
    ZERO no_liquidity       (was ~48%)

INTERPRETATION: with honest pricing the phantom rejection classes vanish
ENTIRELY. What is left is ~10 real candidate cycles per 7 minutes, none of them
profitable. Throughput fell ~17x versus the local-sim runs because most of those
"candidates" were manufactured from quotes claiming more output than the pool
held (~10,000 such quotes per run).

This is the most honest state the pipeline has been in. The question is no longer
"why do candidates fail" — they now fail for the one legitimate reason. It is
"does a profitable cycle exist in our universe, and can we see it in time".

NEXT, in order:
1. Widen the search, not the sizing. 10 cycles/7min is a tiny sample. The
   cross-venue pair count and scan cadence bound what we can find; see the edge
   universe section.
2. Measure the RPC cost of quoter-mode pricing per edge and its effect on
   scan-to-scan latency. Local sim was fast and wrong; quoter is slow and right.
   A quoter-verified CACHE, or quoter-verification only of the top-N ranked
   edges, is the obvious middle path.
3. Populate `quote_block_lag` (still never written) before drawing any conclusion
   about staleness.

DO NOT go back to ARBOT_LOCAL_CL_QUOTES=1 to restore throughput. That throughput
was fictional.


## Widening + latency — 2026-08-27

CHANGES: UNIV3_QUOTE_CONCURRENCY 24 -> 64 (.env);
         max_edges_hot 1000 -> 2500 (ops/inputs.yaml, .bak.widen kept).

LATENCY: WORKED.
    populate_ms  9,733-14,932  ->  ~4,900-5,200 (one 13,670 outlier)   ~2.5x
    rpc_calls    973-1,507     ->  813-1,292
    429          0             ->  0        (headroom remains at 64)

WIDENING: APPLIED BUT INERT.
    "max_edges_hot=2500" confirmed in the derived-capacity log (was 1000).
    Candidates: 10 -> 11 per ~7 min. Unchanged within noise.
    All 11 rejected `no_profitable_size`. Zero plan_build_failed, zero
    no_liquidity — the honest-pricing state holds.

CONCLUSION: edge COUNT is not the binding constraint on candidate volume.
Raising it further will cost RPC and return nothing. Do not keep turning this
knob.

WHAT ACTUALLY BOUNDS CANDIDATE VOLUME (untested, in priority order):
 1. Pool universe COMPOSITION, not size. Only ~180 pairs are cross-venue
    capable (see the edge-universe section) and every candidate this project has
    produced was cross-venue. 141,364 of our 142,573 pools are uniswap_v2, and
    `max_hot_pools_per_venue=1500` caps that venue anyway. Adding depth-verified
    Aerodrome/Pancake/V4 pools on pairs that ALREADY have a UniV3 pool is the
    lever; adding more single-venue pools is not.
 2. Cycle search parameters (`HubSearchLimits`: max_hops, max_cycles,
    parallel_edges_per_pair, timeout). Never measured. `parallel_edges_per_pair`
    defaults to 3 and directly controls how many pool choices per pair the
    search can see.
 3. Scan cadence still ~5s vs 2s blocks. Better than 15s, still 2+ blocks
    behind. Populate remains the dominant term.


## Cross-venue pool addition — 2026-08-27 (attempted, REVERTED)

GOAL: add Aerodrome Slipstream + PancakeSwap V3 pools on pairs that already have
a UniV3 pool, so cross-venue cycles become possible.

METHOD THAT WORKS (reuse this):
  Aerodrome Slipstream CL factory 0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A
    getPool(address,address,int24) = 0x28af8d0b, tickSpacings [1,50,100,200,2000]
  PancakeSwap V3 factory 0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865
    getPool(address,address,uint24) = 0x1698ee82, fees [100,500,2500,10000]
  Both factories verified to have code.

RESULT over the top-45 UniV3 pairs by liquidity:
  95 pools found -> 37 survived two-sided depth (58 dropped for a thin side)
  -> only 8 were NEW (29 of 37 already present in our inventory)
  cross-venue capable pairs: 180 -> 182

KEY FINDING: our inventory ALREADY covers Aerodrome/Pancake on the deep UniV3
pairs. This route is exhausted for the majors. Going further means reaching into
thinner pairs, which are less likely to be profitable anyway.

REGRESSION + REVERT: the run with the 8 added pools produced 0 candidate records
in 6 min; restoring `pools.jsonl.bak` for both venues brought them back (6 in
4 min, matching the 11-in-7-min baseline). Mechanism UNEXPLAINED — the files were
valid JSONL with a schema identical to the originals and the log showed no
errors. Single run per arm, so per this project's own protocol that is NOT
conclusive causality. Backups restored; both venue files are at their pre-change
state.

TOOLING WARNING: a hand-rolled multicall3 aggregate3 encoder/decoder produced
garbage (every pool decoded to 0x...0100, the fee parameter leaking through the
offset math) and briefly reported "373 pools found" which was meaningless. Direct
per-call eth_call was correct. Verify any batch decoder against a known pool
before trusting its output.


## Search-breadth levers — ALL FOUR INERT (2026-08-27/28)

Under honest pricing (ARBOT_LOCAL_CL_QUOTES=0), candidate volume sits at a hard
plateau of ~10-13 per 7 minutes and NOTHING widens it:

    baseline                                  10
    max_edges_hot        1000 -> 2500         11
    cross-venue pools    180 -> 182 pairs     (reverted; already covered)
    parallel_edges_per_pair 3 -> 8            12
    topk_per_token       6 -> 24              13

All within noise. Every run: 100% `no_profitable_size`, zero plan_build_failed,
zero no_liquidity. RPC healthy throughout (429 = 0-1).

CONCLUSION: candidate volume is NOT limited by search breadth, by the per-pair
edge cap, or by the per-start-token cycle cap. Stop turning these knobs — each
costs RPC and returns nothing. Revert them if RPC budget matters:
  max_edges_hot 2500 -> 1000  (ops/inputs.yaml, .bak.widen)
  topk_per_token  24 -> 6     (ops/inputs.yaml, .bak.topk)
KEEP UNIV3_QUOTE_CONCURRENCY=64 — that one genuinely halved populate time with
zero 429s.

WHAT THE PLATEAU MEANS: with ~5s scans (~84 scans per 7 min) and 182 cross-venue
capable pairs, producing ~12 candidates means the overwhelming majority of scans
surface NO cycle worth logging. The limit is upstream of every knob tested — it
is how many cycles the graph contains that clear the pre-filters at all.

REMAINING UNTESTED HYPOTHESES, in order:
 1. Graph structure. 182 cross-venue pairs is the real universe; the other
    135,864 pairs are single-venue and cannot cycle. Growing THAT number means
    sourcing pools on pairs we already trade, from venues we do not yet load
    (Curve, V4 beyond the 70 present, Solidly forks) — not more UniV3.
 2. Scan cadence, still ~5s vs 2s blocks. An arb visible for one block is gone
    before we quote it. Populate remains the dominant term.
 3. `quote_block_lag` is STILL never populated. Until it is, staleness is
    unmeasured and (2) is a hypothesis, not a finding.


## UniV4 inventory format — FIXED (2026-08-28)

THE FILE WAS WRONG IN KIND, THREE TIMES OVER:
  1. `data/base/uniswap_v4/pools.jsonl` held `{pool, token0, token1, fee, ...}` —
     the UniV3 shape — and 14 of 70 entries were literally UniV3 POOL ADDRESSES.
     UniV4 has NO per-pool address.
  2. A later revision held `{poolId, pair, liq_usd_approx}`. `poolId` is the right
     identity but useless for quoting: it is
     keccak256(abi.encode(currency0, currency1, fee, tickSpacing, hooks)) — a
     ONE-WAY hash. The quoter needs the KEY, which cannot be recovered from it.
  3. The container was `.jsonl` (newline-delimited). `parse_pool_configs` expects
     a JSON ARRAY; JSONL fails with "invalid type: map, expected a sequence".

CORRECT SHAPE is `Univ4PoolCfg` (src/venues.rs:458), a JSON array of:
    poolManager, tokenIn, tokenOut, fee, tickSpacing, hooks, sqrtPriceX96
Now at `data/base/uniswap_v4/pools.json` (old file kept as pools.jsonl.bak).
Pinned by `venues::tests::shipped_univ4_inventory_parses_as_poolkeys`, which runs
the SHIPPED file through the real resolver.

RECOVERING A PoolKey FROM A poolId — the method, since it will be needed again:
brute-force the small key space and match the hash. The PoolKey is only
(currency0, currency1, fee, tickSpacing, hooks); vanilla pools have hooks=0x0 and
a handful of fee/spacing combos, so ~49 candidates per pair. Demonstrated:
    cbBTC/USDC poolId 0x12d76c5c.. -> fee=500, tickSpacing=10, hooks=0x0
Live price via StateView 0xA3c0c9b65baD0b08107Aa264b0f3dB444b867A71
`getSlot0(bytes32)` -> sqrtPriceX96 2804694573818449354677886079.
(eth_getLogs on the PoolManager would be the general route, but BlockPI caps it
at 5,000 blocks and V4 spans ~25M blocks on Base, and only one RPC is configured.)

ONLY 1 OF 15 ENTRIES WAS WORTH PORTING. The V4 set is POD, ClawBank, GITLAWB,
Basecat, BASEMATE, BAES, NVDAc, RALLY/ELMT etc — exotic single-venue pairs that
cannot form a cross-venue cycle. Only cbBTC/USDC and USDT/USDC involve tokens we
trade, and both already exist on UniV3 and Aerodrome.

THE VENUE REMAINS CORRECTLY DISABLED. Two independent gates:
  * `BASE_UNIV4_POOLS` / `UNIV4_POOLS` are unset, so the collector never runs.
  * `ENABLE_UNIV4_FIXED_PRICE_QUOTES` is unset, and collect_univ4_edges refuses
    to emit edges without it.
The reason is in the code comment: the V4 quote is a FIXED-PRICE, zero-price-impact
approximation that "produces phantom profits for any non-trivial size". That is
precisely the defect class this project spent weeks removing. DO NOT set that flag
to gain edges. V4 needs a real tick-crossing quote (on-chain Quoter/StateView)
before it can be enabled.

## Cautions

- Do NOT trust `hub_usd_liquidity` — it measures the HUB side only. That is what
  scored the 0.154-bsdETH pool as $622k of depth.
- Do NOT reintroduce a fallback from a refused quote to a more optimistic model.
  Every estimate available at that point (linear secant, single-tick) is MORE
  optimistic than the model that refused.
- `git status` is dirty with all of the above. Review before committing; there
  are also stray untracked scratch files (`base_all_pools_100k.*`,
  `generate_base_venues.py`, `python3 Convert.py`) from earlier sessions.
