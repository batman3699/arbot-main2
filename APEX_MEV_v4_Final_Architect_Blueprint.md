# APEX-MEV v4 — Final Architect Blueprint
## Capture-assurance revision: adaptive multi-chain, event-driven flash-loan arbitrage and MEV execution system with exact state, exact economics, optimal allocation, and adversarial inclusion modeling

**Status:** FINAL / FROZEN ARCHITECTURE — CAPTURE-ASSURANCE HARDENED — supersedes APEX-MEV v3

**Final audit disposition:** v4 is accepted as the architectural baseline only with the Capture Assurance Protocol, signer-sharding, last-mile revalidation, opportunity-coverage auditing, and dispatch invariants defined below. These controls are normative requirements, not optional features.

**Primary objective:** Maximize expected **realized net USD profit per unit time** subject to hard capital, execution, risk, latency, and reliability constraints.

**Economic target:** Build and validate a credible path to **$25,000+ net monthly profit**. This remains a measured capacity target, not a forecast or guarantee.

**Implementation:** Rust off-chain search/execution engine + small typed Solidity executors/adapters.

**Operating philosophy:**

> **Cheap models propose. Exact state and exact AMM mathematics decide. Exact transaction simulation verifies. The execution market decides whether the verified trade lands. Realized P&L recalibrates everything.**

This blueprint is deliberately profit-first. Every subsystem exists because it either increases captured opportunity value, increases landing probability, reduces failure cost, reduces information latency, or lowers execution cost. Anything that cannot demonstrate incremental economic value remains shadow-only or is rejected.

---

# 0. Audit verdict — v3 disposition

APEX-MEV v3 is a serious architecture and already contains the correct foundation: incremental market state, hierarchical route generation, exact AMM math, numerical sizing, bounded allocation, adversarial simulation, EV-based execution, typed Solidity adapters, risk gates, missed-opportunity accounting, and realized-P&L attribution.

It is **not**, however, the final maximum-profit architecture without modification.

The audit identifies eight profit-critical gaps that are upgraded in v4:

1. **Static chain priority is replaced by a dynamic chain-admission and capital/compute allocation layer.** Base remains the engineering and low-latency primary battlefield, but current market activity demonstrates that BSC is large enough to demand first-class economic consideration rather than permanent exclusion. Ethereum remains a high-notional venue. Arbitrum/OP/Unichain remain specialized and measurement-driven until their opportunity surface justifies production resources.[^1][^2]
2. **Base Flashblocks becomes a speculative branch/overlay state engine**, not a single scalar “pending state.” Flashblock index, state root, block hash, ordering lock, divergence, rollback and rebase become first-class state semantics.[^3][^4]
3. **Gas becomes a chain-specific execution-cost model**, including L1 data fees, calldata/compression effects, flashblock capacity, builder/sequencer payment, failure gas and opportunity cost. A generic `gasUsed × gasPrice` model is insufficient on rollups.[^5]
4. **Base inclusion is modeled as a scheduling problem**, because transaction gas limit interacts with Flashblock capacity. A high-gas transaction can be ineligible for earlier Flashblocks even when its fee is high.[^3]
5. **Uniswap v4 is treated as programmable execution**, not “V3 with another fee.” Hooks, dynamic fees, custom accounting, PoolManager deltas and hook-controlled behavior become part of the exact execution fingerprint.[^6]
6. **Flash liquidity becomes a routed resource.** Aave, Morpho, venue-native mechanisms and future approved sources compete on all-in premium, gas overhead, reliability and callback constraints. The engine selects the cheapest reliable source per candidate rather than assuming one lender is canonical.[^7][^8]
7. **EV becomes a scenario-conditioned decision model**, avoiding the false independence assumption in a simple product of probabilities. Correlation among state validity, execution success, competition and inclusion is explicitly represented.
8. **Compute itself is economically allocated.** Candidate generation, exact sizing and adversarial simulation consume scarce CPU and RPC capacity. The system prioritizes work by expected incremental dollars per microsecond / core-second, not by raw candidate count.

The result is **APEX-MEV v4**: a profit-seeking execution system rather than a route-finding bot.

---

# 1. Non-negotiable design principles

## 1.1 Profit is the principal metric

The primary production KPI is:

\[
\boxed{\text{realized net USD per hour}}
\]

The second-order metrics are:

\[
\boxed{\text{expected net USD per eligible candidate}}
\]

and:

\[
\boxed{\text{incremental realized P\&L attributable to each subsystem}}
\]

Everything else is subordinate.

The system must never optimize primarily for:

- number of routes found;
- number of transactions submitted;
- raw spread or raw bps;
- Bellman-Ford iterations per second;
- Hermes queries per second;
- flash-loan notional;
- transaction count;
- theoretical route complexity;
- CPU benchmark scores;
- apparent historical opportunity count without realized capture.

## 1.2 Flash loans are financing, not edge

A flash loan increases accessible notional. It does not create an arbitrage opportunity.

The edge must come from some combination of:

- state freshness;
- structural route discovery;
- superior finite-size pricing mathematics;
- optimal liquidity allocation;
- lower execution cost;
- lower latency;
- better inclusion probability;
- stronger competitor modeling;
- more accurate simulation;
- better execution reliability.

## 1.3 Approximations are candidate generators only

Negative-cycle scores, graph weights, continuous optimization, latency estimates, reference prices and machine-learned predictors may propose actions.

None of them is the final source of economic truth.

The final candidate must survive:

1. exact current or predicted execution state;
2. exact integer pricing;
3. exact flash-loan economics;
4. exact chain-specific fee calculation;
5. exact encoded transaction simulation;
6. adversarial state perturbation where appropriate;
7. current inclusion/ordering model;
8. hard contract profit invariants.

## 1.4 Do not pay for complexity that does not make money

Any module that cannot demonstrate incremental P&L over an appropriate benchmark remains disabled.

The benchmark is not “does the module produce candidates?”

The benchmark is:

\[
\Delta P\&L_{realized} - \Delta infrastructure\ cost - \Delta failure\ cost > 0
\]

with confidence appropriate to the strategy class.

---

# 2. Final system objective function

The v3 objective was:

\[
EV(a)=P_{land}P_{state}P_{exec}P_{net}-C_{failure}
\]

That is directionally correct but can incorrectly imply independence between probabilities that are often correlated.

APEX-MEV v4 therefore uses a **scenario-conditioned expected value model**.

For action \(a\), current information \(I\), and possible future execution states \(s\):

\[
J(a\mid I)=\sum_s P(s\mid I,a)\,\Pi(a,s)-C_{irrecoverable}(a)
\]

where \(\Pi(a,s)\) is the exact or conservatively bounded profit in scenario \(s\).

The scenario tree can include:

- same state, lands immediately;
- same state, lands one Flashblock later;
- competing same-pool swap first;
- competing arbitrage first;
- target backrun state changes;
- transaction delayed beyond validity window;
- route execution success;
- venue revert;
- flash-liquidity failure;
- gas/cost realization above forecast;
- inclusion rejection;
- preconfirmation divergence.

## 2.1 Robust objective

A candidate is eligible only when:

\[
J(a)>0
\]

and:

\[
\Pr(\Pi(a,s)>0) \ge p_{min}
\]

and the downside distribution is acceptable.

For high-risk opportunity classes the gate may also require:

\[
CVaR_{\alpha}(Loss(a)) \le L_{max}
\]

or an equivalent lower-tail profit constraint.

## 2.2 Complete cost model

Define:

\[
NetProfit = GrossReturn
- DEXFees
- FlashFee
- L2ExecutionFee
- L1DataFee
- PriorityFee
- BuilderOrSequencerPayment
- FailureCost
- ExternalExecutionCost
\]

plus any chain-specific cost that directly changes realized P&L.

The model also estimates **opportunity cost** of occupying CPU, RPC simulation slots, private-builder capacity and signing/submission slots when multiple candidates compete for scarce resources.

## 2.3 Candidate eligibility

```text
expected_net_EV > 0
AND
robustness_margin >= threshold
AND
state_freshness <= strategy limit
AND
simulation_fidelity >= required level
AND
execution path is currently healthy
AND
flash liquidity is available
AND
contract route authorization is valid
AND
cost estimate confidence is acceptable
```

## 2.4 Capture-assurance boundary — the strongest truthful guarantee

The phrase “guaranteed capture” must be split into two different claims. The system can guarantee its **own execution behavior** under explicit preconditions; it cannot guarantee an external sequencer/builder will accept a transaction, that a competitor will not arrive first, or that a chain will remain available. Those external outcomes are outside software control.

The mandatory engineering guarantee is therefore:

> **Every opportunity that is detected, economically admissible, simulation-valid, and still inside its execution-validity window must receive a deterministic live dispatch attempt through the permitted lowest-leakage execution path before its dispatch deadline, or transition to an explicit, machine-recorded failure state. It may never disappear silently, wait behind a lower-value candidate, or expire inside an internal queue.**

This is a hard invariant. The system is considered architecturally defective if an otherwise admissible opportunity is lost because of:

```text
internal queueing
missing signer capacity
nonce collision
RPC single-point failure
simulation slot starvation
submission worker starvation
avoidable serialization
late configuration lookup
avoidable redundant recomputation
last-mile state ambiguity that could have been detected earlier
```

Those are engineering failures, not market failures.

## 2.5 Opportunity Ticket — mandatory execution reservation

Every candidate that crosses the economic/risk gate becomes an immutable **Opportunity Ticket** before any live submission work begins.

```rust
struct OpportunityTicket {
    ticket_id,
    chain_id,
    strategy,
    state_fingerprint,
    route_commitment,
    exact_input,
    expected_net_ev,
    robustness_margin,
    validity_start,
    dispatch_deadline,
    target_execution_window,
    signer_lane,
    nonce,
    flash_source,
    simulation_result_hash,
    submission_policy,
    required_gas_limit,
    created_at,
    status,
}
```

Ticket state is monotonic:

```text
OBSERVED
  → RESERVED
  → EXACTING
  → SIMULATED
  → AUTHORIZED
  → SIGNED
  → DISPATCHING
  → ACKNOWLEDGED
  → PRECONFIRMED / INCLUDED
  → FINALIZED
  → RECONCILED
```

Terminal loss states are explicit:

```text
STALE
STATE_CHANGED
EV_COLLAPSED
RISK_REJECTED
DISPATCH_TIMEOUT
NONCE_UNAVAILABLE
SIGNER_UNAVAILABLE
SUBMISSION_REJECTED
COMPETITOR_WON
REVERTED
DIVERGED
```

A ticket receives a hard TTL derived from the opportunity's estimated decay and chain inclusion window. No ticket is allowed to remain queued past `dispatch_deadline`.

## 2.6 Fast path and slow path are mandatory

The architecture must maintain two search planes:

```text
FAST PATH
  pre-materialized route templates
  incremental state patch
  exact affected-pool repricing
  finite-size warm starts
  immediate simulation
  immediate signing/dispatch

SLOW PATH
  broad graph search
  Hermes rebuilds
  exhaustive finite-size discovery
  counterfactual coverage audit
  research strategies
```

The slow path is never allowed to delay the fast path. New state events first wake the affected fast-path route closure; broader discovery runs independently on reserved resources.

## 2.7 Opportunity-coverage auditor

A fast path can only capture what it discovers. To make discovery coverage measurable, APEX maintains a delayed **Coverage Auditor** that performs broader, more expensive discovery against historical/speculative state and compares its best executable candidates with those produced by the hot path.

Track:

```text
hot_path_recall
high_EV_miss_rate
route_class_miss_rate
venue_class_miss_rate
state-trigger miss rate
finite_size_miss_rate
coverage_audit_lag
```

A material unexplained hot-path miss rate automatically expands route templates, raises slow-path resource allocation, or disables the affected strategy class. The purpose is not to pretend the auditor is omniscient; it is to prevent the engine from silently losing known opportunity classes.

## 2.8 Capture-assurance resource reservation

A live ticket reserves, before signing:

```text
one signer/nonce lane
one simulation completion slot
one submission lane
minimum native gas balance
flash-source feasibility
required executor authorization
required state-read capacity
```

This prevents the classic failure mode in which the system discovers more profitable opportunities than its own execution infrastructure can physically dispatch.

For independent opportunities, multiple signer lanes are mandatory rather than serializing all live execution through a single nonce stream.

## 2.9 External guarantee boundary

The system therefore reports two separate metrics:

```text
SYSTEM_CAPTURE_ASSURANCE
= fraction of admitted tickets that were dispatched within deadline

MARKET_CAPTURE
= fraction of economically available opportunities actually landed
```

The first can and should approach 100% subject to explicit non-market infrastructure failures. The second is the competitive market outcome and is optimized—not falsely guaranteed.

---

## 2.10 Numeraire and USD-mark isolation

Execution correctness is never dependent on a single external USD price feed. The economic core keeps the trade in exact token units and, where useful for cross-chain comparison, converts to USD using a bounded valuation interval:

\[
USDValue \in [V_{low}, V_{high}]
\]

A candidate must remain economically valid under the configured conservative valuation bound. USD marks are primarily a portfolio-accounting and resource-allocation layer; they are not a substitute for exact on-chain balances, debt repayment, or token-denominated profit invariants.


# 3. Economic battlefield — dynamic chain portfolio

## 3.1 Why v4 changes the chain model

The v3 “Base-first” choice remains correct as an engineering anchor because Base exposes ~200 ms Flashblocks and preconfirmation-aware state. However, maximum profit requires the system to allocate engineering and compute capacity according to **measured opportunity economics**, not a permanent ideological chain ranking.[^3][^4]

Current observed DEX activity is large on multiple chains. A recent snapshot showed Base around **$1.06B 24h / $26.89B 30d**, Ethereum around **$1.22B 24h / $36.93B 30d**, and BSC materially larger than Base in the same current market window in other DefiLlama snapshots. These figures are only a prior: DEX volume is not equivalent to atomic arbitrage EV, and the engine must measure opportunity density and capture directly.[^1][^2]

## 3.2 Chain portfolio

Production candidates:

```text
Base         low-latency primary development / production battlefield
Ethereum     high-notional, high-value execution market
BSC          first-class economic candidate; Pancake-heavy route surface
Arbitrum     specialized high-frequency candidate; runtime ordering discovery required
OP Mainnet   specialized Flashblocks/low-latency candidate; volume presently lower
Unichain     research/shadow candidate until opportunity density justifies production
```

The system does **not** hard-code this ordering forever.

## 3.3 Chain admission score

At rolling intervals calculate:

\[
ChainScore_c=
\frac{E[NetUSD/hour\mid c]}{InfraCost_c+MarginalComputeCost_c+RiskAdjustedFailureCost_c}
\]

with minimum observation and reliability requirements.

A chain with more volume but poor capture can rank below a lower-volume chain with a specialized execution edge.

## 3.4 Chain portfolio controller

The controller allocates:

- state ingestion capacity;
- candidate generation CPU;
- exact simulation capacity;
- dedicated node/RPC connections;
- signer capacity;
- flash-loan source evaluation;
- engineering effort.

Use a constrained online allocator with hard live-risk floors and shadow exploration. Exploration is performed preferentially in shadow or low-notional canary modes; live trading remains exploitative once safety gates are met.

---

# 4. Chain-specific execution regimes

A universal execution interface is required, but execution economics are chain-specific.

```text
ChainExecutionAdapter
├── state_feed()
├── pending_state()
├── simulate()
├── estimate_total_fee()
├── estimate_inclusion_probability()
├── optimize_submission_cost()
├── submit()
├── replacement_policy()
├── observe_outcome()
└── reconcile_final_state()
```

## 4.1 Base

Base is a **preconfirmation-aware sequencing market** using Flashblocks.

Relevant official interfaces include Flashblocks transaction/log/full-block streams, `pending`, `base_transactionStatus`, and simulation facilities.[^3][^4]

Critical v4 difference: the engine models **Flashblock eligibility and capacity** in addition to fee ordering.

A transaction whose gas limit exceeds the capacity of the earliest Flashblock may be forced into a later Flashblock. Base documentation explicitly describes this behavior for high-gas transactions.[^3]

Once a Flashblock is built and broadcast, its transaction ordering is locked; a later higher-priority transaction cannot retroactively enter that earlier Flashblock.[^3]

Therefore the Base submission decision is:

```text
current Flashblock index
        ↓
estimated residual gas capacity
        ↓
transaction gas limit
        ↓
earliest eligible Flashblock
        ↓
fee rank within eligible capacity
        ↓
state validity at that time
        ↓
submit / reject
```

## 4.2 Ethereum

Ethereum uses private-builder / relay execution relationships and an empirical bid-to-inclusion model.

The engine supports multiple builder destinations where economically useful and chooses the submission set according to expected net value, builder acceptance, historical behavior and leakage risk.

## 4.3 Arbitrum

Arbitrum execution must be configured from the **live ordering regime**, not from a hard-coded historical assumption.

The 2026 governance process describes a transition toward PGA and a Fast Feed with approximately 125 ms ordering rounds, but the blueprint must treat the active mode as runtime state because the finalized governance record describes activation through subsequent execution and toggles.[^9][^10]

At process startup and periodically thereafter, the adapter discovers:

```text
ordering_mode
priority_fee semantics
PGA round length
Fast Feed availability
private sequencer feed availability
transaction replacement rules
current gas / data fee model
```

Supported modes are conceptually:

```text
TIMEBOOST
PGA
PGA+FAST_FEED
OTHER_CURRENT_MODE
```

## 4.4 OP Mainnet

OP Stack fees contain an L1 data component in addition to execution costs. The current fee model depends on compressed transaction size and Ethereum base/blob fee conditions, so calldata size becomes an economic optimization variable.[^5]

APEX therefore models:

```text
L2 execution fee
+ L1 data fee
+ priority fee
+ failure cost
```

rather than only `gasUsed × gasPrice`.

## 4.5 BSC

BSC is admitted as a first-class economic candidate but does not receive a privileged status without measured capture.

The chain adapter discovers:

- active public/private ordering options;
- RPC propagation behavior;
- fee/priority semantics;
- available flash liquidity;
- dominant DEX inventory;
- pool update cadence;
- competitor intensity.

## 4.6 Unichain

Unichain supports incremental Flashblocks-style execution architecture, but current spot DEX activity is materially lower than Base/Ethereum/BSC in the observed snapshot. It therefore remains shadow or specialized until measured opportunity density justifies live capacity.

---

# 5. State acquisition — canonical plus speculative branches

The v3 design correctly moved away from full graph rescans. v4 goes further: **state is versioned and branchable**.

## 5.1 State layers

```text
CanonicalState
   │
   ├── ConfirmedState
   │
   └── SpeculativeStateTree
          ├── Flashblock / pending branch 0
          ├── Flashblock / pending branch 1
          ├── Target-event branch
          └── Candidate-local execution branch
```

## 5.2 State fingerprint

Each state version carries:

```text
chain_id
parent_block_hash
confirmed_block_number
preconf_sequence
flashblock_index
state_root_or_verified_equivalent
block_hash_if_available
state_delta_hash
venue_state_version
external_dependency_fingerprint
```

The final stable `diff` fields should be preferred over unstable convenience metadata when using Base Flashblocks payloads.[^3]

## 5.3 Patch engine

```text
incoming tx/log/flashblock
        ↓
classify state mutation
        ↓
identify pools/contracts
        ↓
apply delta to immutable snapshot
        ↓
update affected dependency closure
        ↓
publish new state version
```

No global mutable state is used as shared truth between search workers.

Workers receive immutable snapshots or versioned read handles.

## 5.4 Rollback and divergence

When the final block differs from speculative Flashblocks:

```text
speculative branch
      ↓
compare final state fingerprint
      ↓
if equal → commit
if different → rollback affected branch
      ↓
rebuild canonical snapshot
      ↓
re-evaluate impacted candidates
```

Measure:

```text
preconf_to_final_divergence_rate
state_branch_reorg_count
rollback_cost_ms
candidate_invalidated_by_divergence
```

## 5.5 Dependency indexes

Maintain all v3 relationships plus state-version dependencies:

```text
pool → token pair
pool → routes
pool → candidate cycles
pool → state variables
pool → venue adapter
pool → active branch versions

token → pools
token → cycles
cycle → pools
cycle → loan assets
cycle → conflict resources
candidate → state fingerprint
candidate → simulation state
candidate → submission state
candidate → execution commitment
```

## 5.6 Feed integrity and gap recovery

A real-time state engine is only as correct as the continuity of the feed that drives it. Every state stream therefore has explicit sequence, ordering and gap detection.

Track:

```text
feed_sequence
last_sequence
expected_sequence
gap_count
out_of_order_count
duplicate_count
reconnect_count
recovery_time_ms
feed_freshness_ms
state_rebuild_after_gap
```

Rules:

```text
sequence gap detected
      ↓
mark affected state UNSAFE
      ↓
stop new tickets dependent on that branch
      ↓
parallel backfill / state reconstruction
      ↓
verify state fingerprint
      ↓
resume trading
```

For capture-critical chains, maintain at least two independent transport paths where available. Redundancy exists to repair transport failure, not to merge contradictory states. A state version becomes authoritative only after its parentage and fingerprint are verified.

A feed gap may never be silently converted into “probably unchanged.”

For capture-critical execution, each usable state version carries:

```text
parent_state_id
state_fingerprint
feed_source_id
sequence_range
first_seen_ns
last_seen_ns
canonicality_status
reconstruction_status
```

Only a state with `reconstruction_status = VERIFIED` may authorize a live ticket. If two feeds disagree, neither is promoted by majority vote: the controller resolves the disagreement by parentage, sequence continuity, explicit reconciliation, or full state rebuild.

---

# 6. Market graph and pool state model

The graph is a **candidate-generation representation**, not the execution model.

## 6.1 Edge representation

An edge represents a venue-specific transformation:

\[
(u,p,v): x_u \rightarrow f_p(x_u)=x_v
\]

For graph search, approximate effective rates can be represented as:

\[
w_{uv}=-\ln(r_{uv}^{eff})
\]

Negative cycles are then candidate generators.

## 6.2 Finite-size warning

A negative infinitesimal cycle can become unprofitable at executable size because of:

- price impact;
- concentrated liquidity boundaries;
- fee tiers;
- dynamic fees;
- gas;
- L1 data fees;
- flash-loan fees;
- discrete rounding;
- shared pool coupling;
- competitor ordering.

The exact finite-size path is authoritative.

## 6.3 Pool admissibility

Each pool must have:

```text
verified address
verified venue
verified token pair
verified decimals
verified fee behavior
state reconstruction method
liquidity/depth estimate
gas profile
revert profile
transfer semantics
block/event update mapping
```

Low-liquidity pools are rejected by configurable economic thresholds, not a permanent magic number.

---

# 7. Token admission and token semantics

The initial highly liquid Base universe from v3 remains a good seed:

```text
WETH
USDC
cbBTC
cbETH
wstETH
AERO
DAI
USDS
EURC
```

The universe remains dynamic.

## 7.1 Token admission model

A token is eligible when it satisfies measured thresholds for:

- liquidity;
- observed volume;
- pool connectivity;
- state reconstruction reliability;
- transfer safety;
- price stability appropriate to its route class;
- realized arbitrage density.

## 7.2 ERC-20 semantics classifier

Before production use, classify:

```text
standard ERC-20
fee-on-transfer
rebasing
blacklist-enabled
pauseable
non-standard approve behavior
decimals anomalies
permit anomalies
balance-changing hooks / wrappers
```

For non-standard tokens, expected transfer output must be derived from **actual balance deltas** in simulation rather than nominal transfer amounts.

## 7.3 Token risk fingerprint

```text
token_address
code_hash
proxy_implementation
transfer_semantics
allowance_semantics
decimals
pause_state
admin/controller metadata where relevant
```

A material code or behavior change invalidates affected route assumptions.

---

# 8. Venue universe and adapter architecture

## 8.1 Initial Base universe

Retain the v3 priority venues:

```text
Aerodrome
Aerodrome Slipstream
Uniswap V3
Uniswap V4
PancakeSwap
Aave V3 (flash liquidity)
```

Additional venues may be admitted through the same process.

## 8.2 Venue admission gate

```text
deployment verified
ABI verified
pool discovery verified
state reconstruction verified
exact pricing differential-tested
gas profile measured
revert taxonomy implemented
adapter fuzz-tested
multi-day shadow test
micro-canary only where economically justified
```

Every venue gets an independent circuit breaker.

## 8.3 Typed adapter contract

Rust:

```rust
trait VenueAdapter {
    fn identify_state_dependencies(&self, pool: &PoolId) -> StateDeps;
    fn quote_exact(&self, state: &StateSnapshot, order: &Order) -> ExactQuote;
    fn simulate_call_graph(&self, candidate: &Candidate) -> CallGraph;
    fn gas_model(&self, candidate: &Candidate) -> GasModel;
    fn classify_revert(&self, data: &[u8]) -> RevertClass;
    fn encode_exact(&self, candidate: &Candidate) -> EncodedAction;
}
```

No adapter may hide meaningful economic assumptions from the core engine.

---

# 9. Uniswap V3 exact engine

V3-like concentrated liquidity remains an exact integer state machine.

Required state includes at minimum:

```text
sqrtPriceX96
liquidity
current_tick
initialized_ticks
fee_tier
fee_growth/state variables required for exact execution
```

Pricing must reproduce the protocol's integer rounding semantics, including the exact `SqrtPriceMath` behavior.[^11]

Outputs:

```text
amount_out
fee
price_impact
crossed_ticks
next_state
state_dependencies
estimated_gas
revert_risk
```

No router quote is authoritative.

---

# 10. Uniswap V4 exact engine — upgraded for programmable pools

This is one of the most important v3→v4 changes.

Uniswap v4 uses a singleton `PoolManager`, hooks, flash accounting, custom accounting and potentially dynamic fees. Therefore a v4 pool cannot be represented as only:

```text
(token0, token1, fee, sqrtPrice, liquidity)
```

## 10.1 Required v4 state

```text
PoolKey
currency0
currency1
fee mode
hook address
hook permissions
current pool state
liquidity
initialized ticks
hook code hash / implementation fingerprint
hook-controlled state dependencies
external oracle/reference state if consulted
custom accounting behavior
```

## 10.2 Hook execution model

The exact engine must account for the possibility that:

```text
beforeSwap
        ↓
fee modification / custom logic
        ↓
core swap
        ↓
afterSwap
        ↓
custom accounting / return deltas
        ↓
final settlement
```

Hooks can change fees and execution behavior, so they are part of the state machine rather than mere metadata.[^6]

Uniswap v4 also uses singleton `PoolManager` flash accounting in which intermediate operations update internal deltas and only final balance changes require token transfers. Exact modeling must therefore reproduce the lock/unlock and settlement semantics, not merely individual pool swap mathematics.[^17]

## 10.3 V4 quote authority

The only authoritative quote is execution-equivalent state transition:

\[
S_{next}=F_{v4}(S, order, hook\ state, dependencies)
\]

The engine rejects a route if a required hook dependency cannot be reconstructed with sufficient fidelity.

## 10.4 V4 execution isolation

A production v4 route must carry:

```text
hook address
hook fingerprint
hook simulation tier
hook dependency freshness
custom-accounting flag
native/ERC-20 settlement mode
```

If the hook is effectively unmodelled, the route is shadow-only.

For v4, the executor must also model `PoolManager` lock/unlock and delta settlement exactly. Flash accounting tracks internal net deltas during the unlocked execution context and requires those deltas to be resolved before the context closes; therefore intermediate balances must never be mistaken for settled balances.[^17]

---

# 11. Exact stable-swap / CPMM / venue-specific engines

The pricing core supports:

```text
CPMM
stable-swap / invariant curves
concentrated liquidity
Aerodrome / Slipstream-specific math
Pancake concentrated liquidity
Uniswap V4 programmable execution
venue-specific custom curves where formally reconstructed
```

Every engine must define:

```text
quote_exact()
next_state_exact()
fee_exact()
rounding_exact()
revert_conditions()
state_dependencies()
```

For any engine not proven exact, the output is candidate-only.

---

# 12. Candidate generation stack

Candidate generation is intentionally redundant.

## 12.1 Engine A — incremental negative-cycle search

Use incremental weighted graph updates after pool state changes.

Recommended implementation family:

```text
incremental Bellman-Ford
modified Moore-Bellman-Ford
negative-cycle extraction
Top-K negative-cycle candidates
```

The graph layer is allowed to be approximate because it only proposes routes.

## 12.2 Engine B — structure-aware routing

Hermes remains a route-generation accelerator, especially where the graph exhibits stable structure and query volume is high.[^12]

Maintain:

```text
Hermes candidate rank
full-search rank
certificate outcome
miss rate
structure age
rebuild cost
```

When Hermes repeatedly misses materially superior routes, automatically reduce its authority and trigger structural rebuild/re-evaluation.

## 12.3 Engine C — finite-size route search

Add a route-generator that searches directly for finite-size improvement rather than relying only on infinitesimal rates.

Candidate families:

```text
pairwise cross-venue mismatch
k-shortest simple routes
k-shortest cycles
same-pair split opportunities
event-targeted routes
backrun templates
liquidation routes
stable/correlated dislocations
```

This engine is particularly important for route classes where graph weights are misleading because nonlinear execution dominates.

## 12.4 Engine D — event templates

Target:

```text
large swap
liquidity removal
liquidity addition
liquidation
oracle-sensitive mutation
stablecoin dislocation
tick transition
hook state mutation
fee-tier / dynamic-fee change
```

## 12.5 Engine E — backrun prediction

For an incoming target transaction:

```text
target transaction
      ↓
predict target-induced state delta
      ↓
exactly simulate target
      ↓
reprice affected closure
      ↓
search immediate profitable successor actions
```

The target transaction itself is never assumed final until the execution regime says it is.

---

# 13. Route topology policy

V3 default:

```text
3 hops = preferred
4 hops = allowed
>4 hops = exceptional
```

Ethereum:

```text
3 hops = preferred
4 hops = allowed
5 hops = exceptional
```

The more important v4 rule is that **hop count is not the true complexity metric**.

Use:

\[
ComplexityCost = f(hops, calls, calldata, stateDeps, tickCrossings, hooks, gas, failureSurface)
\]

A four-hop path with simple CPMM state can be economically safer than a two-hop v4 route containing expensive or uncertain hook logic.

---

# 14. Exact route sizing

## 14.1 Primary sizing objective

For route \(p\):

\[
\max_{x \ge 0} \left[f_p(x)-Cost_p(x)\right]
\]

subject to:

```text
flash liquidity
pool depth
slippage bounds
loan availability
execution gas
chain fee constraints
deadline
profit floor
```

## 14.2 Continuous candidate sizing

Continuous methods are used as warm starts:

- Newton-Raphson where derivatives are valid;
- Brent's method for one-dimensional unimodal searches;
- bracketed search;
- local gradient / marginal search.

The exact candidate is then verified using integer token units.

## 14.3 Final discrete refinement

```text
continuous optimum
       ↓
local neighborhood
       ↓
integer / wei candidates
       ↓
exact AMM evaluation
       ↓
exact encoded EVM simulation
       ↓
maximum robust EV
```

This prevents continuous mathematics from becoming an execution dependency.

---

# 15. Parallel-pool flow splitting

For pools \(i=1...n\):

\[
\sum_i x_i=X
\]

and maximize:

\[
\sum_i f_i(x_i)-C_{multi}(x)
\]

## 15.1 Concave region

For genuinely concave segments:

\[
f_i'(x_i)=\lambda
\]

for all active pools.

The KKT solution is a warm start.

## 15.2 Concentrated-liquidity boundaries

Segment the problem at known tick boundaries.

```text
continuous allocation
       ↓
tick-boundary discovery
       ↓
piecewise candidate set
       ↓
discrete allocation refinement
       ↓
exact transaction simulation
```

## 15.3 Shared-pool coupling

This is another key v4 upgrade.

The path-separable approximation is invalid when multiple candidate routes materially consume the same pool state.

Therefore:

```text
shared pool detected
      ↓
form shared-pool cluster
      ↓
aggregate pool input
      ↓
exact joint state transition
```

Do not independently optimize two paths against the same pool and then simply sum their profits.

---

# 16. Joint route allocation and improving-path certification

The v3 design correctly treated convex optimization as a warm start. v4 makes the validity criteria explicit.

## 16.1 Valid domain

Joint allocation may use a convex optimization formulation only when:

```text
objective is concave in allocation variables
AND
constraints are convex
AND
fixed activation costs are either absent or separately enumerated
AND
shared-pool coupling is explicitly represented
AND
there are no unmodelled discrete hooks/ticks that change the domain
```

The 2026 multi-path routing literature explicitly distinguishes path-separable concave models from the harder shared-pool / activation-cost cases.[^13]

## 16.2 Improving-path check

After obtaining a candidate allocation, search for an improving move.

If an improving move exists, the allocation is not locally certified.

If the mathematical assumptions for a certificate are violated, label the result:

```text
PROVEN
HEURISTIC
INVALID_FOR_CERTIFICATION
```

Never silently promote a heuristic allocation to “optimal.”

## 16.3 Gas-aware allocation

A mathematically superior split is rejected when:

\[
\Delta OutputValue
\le
\Delta DEXFees+
\Delta Gas+
\Delta L1DataFee+
\Delta FailureCost+
\Delta InclusionCost
\]

---

# 17. Cross-cycle portfolio allocation and packing

The system may retain the best several individually profitable candidates, but the packing layer is deliberately bounded.

## 17.1 Candidate portfolio

After filtering:

```text
top candidates by robust EV
state freshness
venue health
shared-resource conflicts
submission feasibility
```

## 17.2 Conflict graph

Node = candidate action.

Edge = economic/execution conflict.

Conflicts include:

- same pool;
- materially shared token balance;
- incompatible loan asset;
- shared liquidity cap;
- ordering dependency;
- overlapping state mutation;
- excessive calldata/gas;
- same signer/nonce bottleneck;
- same Flashblock capacity bottleneck.

## 17.3 Packing rule

Do **not** choose a packed transaction merely because:

\[
EV_{packed}>EV_{single}
\]

Instead require:

\[
EV_{packed}^{risk-adjusted}
>
EV_{best\ alternative}+Margin
\]

including:

- increased gas limit;
- delayed earliest eligibility on Base;
- larger calldata/L1 data fee;
- larger revert surface;
- additional state dependencies;
- larger capital/flash source requirements.

## 17.4 Search bound

Use:

```text
top few candidates
small compatible subsets
2–3 orderings where needed
exact simulation
```

No unrestricted global integer/nonlinear program is permitted in the hot path.

---

# 18. Strategy stack

## 18.1 Strategy A — triangular arbitrage

Baseline high-frequency engine.

```text
WETH → stable → asset → WETH
```

Use multiple venue combinations, not a single fixed DEX ordering.

Triangles provide the baseline against which more advanced optimizations are measured.

## 18.2 Strategy B — short multi-hop

Three- and four-hop routes where the extra transition materially improves finite-size net profit.

Prefer:

```text
high liquidity
low total cost
low state dependency
high capture probability
```

over raw spread.

## 18.3 Strategy C — event-driven backruns

State event:

```text
incoming transaction / liquidity mutation
      ↓
target classification
      ↓
state prediction
      ↓
exact target simulation
      ↓
post-event route generation
      ↓
inclusion model
      ↓
submit
```

## 18.4 Strategy D — liquidation + unwind

Model protocol-specific details including:

```text
health factor
oracle state
liquidation eligibility
close factor
liquidation bonus
liquidation caps
isolation / collateral constraints
available debt/collateral liquidity
unwind path
flash liquidity cost
execution gas
competition
```

Liquidations get an isolated resource budget.

## 18.5 Strategy E — correlated/stable dislocations

Monitor:

```text
USDC / USDT / DAI / USDS / EURC
WETH / cbETH / wstETH
cbBTC / BTC wrappers
```

External venues may be used as **signals**, never as on-chain execution truth.

Reject apparent dislocations caused by:

- stale feeds;
- oracle lag;
- low liquidity;
- untradeable token behavior;
- state-version mismatch.

## 18.6 Strategy F — finite-size inventory mismatch

An additional route family explicitly hunts situations where a pool's marginal price remains favorable at finite size even though infinitesimal graph rates are uninformative.

This strategy is especially useful for concentrated liquidity and uneven cross-venue depth.

---

# 19. Flash-loan / flash-liquidity router

The flash source is an optimization variable.

## 19.1 FlashSource model

```rust
struct FlashSourceQuote {
    provider: FlashProviderId,
    asset: AssetId,
    amount: U256,
    premium: Amount,
    gas_overhead: GasEstimate,
    callback_constraints: CallbackConstraints,
    availability_probability: f64,
    state_dependencies: StateDeps,
    reliability_score: f64,
}
```

## 19.2 Source candidates

Initial architecture may admit:

```text
Aave V3
Morpho flash liquidity where supported
venue-native / protocol-native flash mechanisms where exact and audited
future approved providers
```

Aave documentation provides the canonical V3 flash-loan interfaces, and current address books are preferable to hard-coded hand-maintained addresses.[^7]

Morpho's flash-loan mechanism is also atomic and imposes explicit callback repayment semantics.[^8]

## 19.3 Selection rule

Choose:

\[
FlashSource^* = \arg\min \left(
FlashFee + GasOverhead + FailureRiskCost + AvailabilityPenalty
\right)
\]

subject to required asset and amount.

## 19.4 Multi-source fallback

The engine does not make the flash provider a single point of failure.

However, the execution contract must support only explicitly approved and tested providers.

---

# 20. Exact simulation hierarchy

## Tier 0 — analytic filter

```text
cheap exact/near-exact AMM math
fee checks
coarse cost model
rough EV
```

Reject obvious losers.

## Tier 1 — exact local state simulation

Use versioned reconstructed state and exact integer venue math.

## Tier 2 — full EVM transaction simulation

Simulate the exact encoded call graph against the relevant fork/preconfirmation state.

Check:

```text
success
revert data
gas
balances
loan repayment
profit invariant
token residues
state changes
```

## Tier 3 — adversarial inclusion simulation

Perturb the future state according to empirically observed competitor behavior:

```text
same-pool same-direction swap
same-pool opposite-direction swap
larger/smaller competitor
competing arbitrage
target transaction ordering change
submission delay
next Flashblock delay
fee escalation
```

## Tier 4 — controlled production validation

Used only for new adapter classes, new strategy families or uncertain execution paths where the expected information gain justifies cost.

It is not a latency technique and is never used as a substitute for simulation.

---

# 21. Adversarial competition model

## 21.1 CompetitorModel

```rust
struct CompetitorModel {
    arrival_lag_distribution,
    observed_sizes,
    opportunity_class_win_rate,
    state_age_to_capture_curve,
    private_flow_intensity,
    builder_acceptance_profile,
    sequencer_acceptance_profile,
    historical_bid_curve,
    ordering_mode,
    observed_reaction_to_target_events,
}
```

## 21.2 Latency buckets

```text
0–20 ms
20–40 ms
40–80 ms
80–120 ms
120–200 ms
200–400 ms
400+ ms
```

These are measurement buckets, not promises.

## 21.3 Outcome censoring

A missed trade does not reveal the exact competitor's action.

Therefore the model should support censored observations:

```text
opportunity existed
submission lost
competitor action unobserved
```

Do not fabricate competitor size from missing observations.

## 21.4 Capture curve

Estimate:

\[
P_{capture}=F(stateAge, latency, fee, gasLimit, strategy, venue, chain, competition)
\]

and update from observed outcomes.

---

# 22. Base Flashblocks execution model — critical upgrade

Base's ~200 ms Flashblock cadence is a first-class signal, not just a feed.[^3][^4]

The current Base documentation exposes the Flashblocks-aware RPC methods on `https://mainnet.base.org` and recommends application-level access through a Flashblocks-aware RPC provider. The raw sequencer infrastructure stream is explicitly for node operators; APEX may consume it only through its own properly configured node integration.[^3]

## 22.1 Flashblock scheduler state

```text
current_flashblock_index
current_block_number
current_block_gas_limit
current_flashblock_gas_budget
residual_gas_capacity
candidate_gas_limit
earliest_eligible_flashblock
fee_rank_estimate
state_validity_window
```

## 22.2 Eligibility logic

The engine computes eligibility from the current builder/sequencer policy and gas limit. It does **not** hard-code a fixed one-tenth rule as an immutable constant because chain parameters and implementation details may change.

For a candidate with gas limit \(G_t\), current capacity model \(B\), and measured Flashblock allocation policy \(Q(k)\), solve:

\[
 k_{eligible}=\min\{k: G_t\le Q(k)\}
\]

then evaluate whether the opportunity survives until that Flashblock.

## 22.3 Ordering lock

Once a Flashblock is built, its ordering is treated as fixed.[^3]

This eliminates an entire class of invalid “pay more later and still land earlier” assumptions.

## 22.4 Backrun timing

A target transaction arriving in Flashblock \(i\) creates a successor-state candidate whose earliest possible execution is constrained by the remaining sequencer process.

The backrun engine therefore evaluates:

```text
target Flashblock index
current residual capacity
next eligible slot
next-block transition
competition
state decay
```

---

# 23. Gas and total transaction cost engine

## 23.1 Universal cost schema

```rust
struct TotalExecutionCost {
    l2_execution_fee,
    l1_data_fee,
    priority_fee,
    builder_payment,
    sequencer_payment,
    flash_fee,
    dex_fees,
    expected_failure_cost,
    calldata_bytes,
    compressed_data_estimate,
    gas_limit,
    gas_used_distribution,
}
```

## 23.2 L2 fee models

OP Stack chains require explicit L1 data-fee modeling based on transaction size/compression and Ethereum fee conditions.[^5]

Arbitrum and other rollups similarly require chain-native data-fee modeling rather than a generic EVM gas formula.

## 23.3 Calldata optimizer

For L2s where data cost is material:

```text
route encoding
↓
calldata size
↓
compression estimate
↓
L1 fee
↓
net EV
```

This makes calldata layout economically relevant.

## 23.4 Gas-limit vs gas-used

Keep separate:

```text
gas_limit = inclusion/scheduling variable
gas_used  = realized cost variable
```

Conflating them is incorrect on systems where gas limit affects scheduling.

---

# 24. Submission optimization

## 24.1 Base

Optimize:

```text
gas limit
priority fee
submission timing
earliest eligible Flashblock
state freshness
```

Do not assume higher priority fee always dominates if the transaction is capacity-ineligible for the current Flashblock.

## 24.2 Ethereum

Optimize:

\[
EV(b)=P_{land}(b, builder, state, time)\times Profit(b)-Cost(b)
\]

Use empirical bid curves rather than static “percentage of profit” rules.

Multiplex to multiple builders/private routes only when incremental inclusion probability exceeds the leakage/complexity cost.

## 24.3 Arbitrum

Use runtime-discovered PGA/Timeboost/Fast Feed behavior.

When PGA is active, fee bidding becomes a high-frequency auction variable; when another ordering regime is active, use that regime's measured rules.

## 24.4 No public leakage by default

Public submission is the fallback, not the default, for routes where private sequencing relationships materially increase EV.

No probabilistic spam.

No uncommitted gas-burning probes.

## 24.5 Last-mile dispatch protocol — mandatory

Once a ticket is `AUTHORIZED`, the system executes the shortest valid path from authorization to network receipt. No nonessential database write, configuration lookup, recomputation, or slow-path work may sit between authorization and dispatch.

The dispatch contract is:

```text
AUTHORIZED
   ↓
reserve nonce/signing lane
   ↓
cheap last-mile validation
   ↓
sign exactly committed payload
   ↓
primary execution endpoint
   ↓
receipt / acknowledgement check
   ↓
controlled fallback if acknowledgement misses deadline
```

### Base

Submit the signed transaction through a Flashblocks-aware Base RPC provider endpoint. Do not connect the application directly to the raw Flashblocks infrastructure stream; that stream is for node operators. Immediately check `base_transactionStatus` or equivalent acknowledgement and watch the Flashblocks-aware transaction stream. Base documents explicitly expose pre-confirmed `pending` state, `eth_simulateV1`, `base_transactionStatus`, and `newFlashblockTransactions` for this purpose.[^3][^15][^16]

Controlled RPC redundancy is permitted using the **same signed transaction**; redundancy is for transport reliability, not for creating multiple economically distinct attempts. If the opportunity has become stale, the fallback is cancelled before dispatch.

### Ethereum

For private execution, dispatch the committed transaction/bundle in parallel to the approved builder paths that have positive incremental expected value. Flashbots supports atomic bundles, block validity windows, registered-builder targeting and replacement/cancellation mechanisms.[^14]

The submission router must treat builder multiplexing as an inclusion-optimization problem, not as blind duplication.

### Arbitrum

When PGA/Fast Feed is active, dispatch timing and priority fee are first-class variables. The current finalized governance specification describes 125 ms PGA rounds, timestamp tie-breaking, priority-fee ordering and explicit block/calldata capacity constraints; the production adapter must discover the active configuration rather than hard-code historical values.[^9][^10]

### OP Stack

Submission and cost logic remain chain-specific. OP Stack chains do not expose the same public-mempool model as Ethereum, and current fee accounting includes execution, L1 data and operator components.[^5]

### BSC / other chains

Use the chain's native low-latency and private submission mechanisms where verified. The adapter must never assume Ethereum-style public mempool behavior without measuring it.

## 24.6 Last-mile revalidation

Immediately before signing, perform only checks that can invalidate the ticket and can execute within the remaining deadline:

```text
chain id
executor code/version fingerprint
critical pool state fingerprint
critical hook fingerprint where applicable
nonce still owned by ticket
fee ceiling
gas-limit eligibility
signer gas balance
flash-source availability
deadline
minimum profit
```

If any critical input changed, the ticket is **re-simulated or invalidated**, never blindly dispatched.

For Base, do not assume a generic `eth_call` against `pending` provides a perfect current block-context triple. Base documents that block-context properties can reflect cached historical context; capture-critical simulation should prefer `eth_simulateV1` with explicit state/block controls and validation enabled where supported.[^15][^16]

## 24.7 Gas-limit minimization as a capture control

For chains where gas limit affects scheduling, gas-limit optimization is part of capture—not merely cost accounting. Define:

\[
G_{safe}=Q_{gas}(simulated\ gas)+headroom
\]

Choose the smallest `gas_limit >= G_safe` that remains valid for execution and fits the earliest economically valuable inclusion window. If no safe gas limit satisfies the scheduling/cost constraints, reject the ticket before signing.

On Base, this directly addresses the documented interaction between transaction gas limit and Flashblock capacity.[^4]

## 24.8 Acknowledgement is not inclusion

The submission controller must distinguish:

```text
transport_accepted
node_known
sequencer_received
builder_acknowledged
preconfirmed
included
finalized
```

An RPC success response only proves that a submission endpoint accepted the request; it is not proof of network receipt, ordering, preconfirmation or inclusion. Every stage has its own timeout and escalation rule.

For Base, `base_transactionStatus = Known` is evidence that the preconfirmation node has received the transaction, not evidence that it has been placed in a Flashblock.[^16]

For Ethereum, builder/relay acknowledgement is evidence of submission/acceptance at that layer, not proof that a proposer selected the bundle. Inclusion remains an observed outcome.[^14]

---

# 25. Transaction commitment and duplication control

Every candidate gets a deterministic commitment hash containing:

```text
chain_id
executor_version
venue/version fingerprints
flash_source
state_fingerprint
route_hash
exact input sizes
min profit
slippage constraints
deadline
submission policy
```

Conceptually:

\[
commit = keccak256(all\ critical\ trade\ parameters)
\]

The executor and off-chain signer must agree on the commitment.

This prevents:

- accidental route mutation after authorization;
- stale candidate submission;
- duplicate attempts;
- wrong-chain execution;
- unexpected venue substitution.

---

# 26. Solidity executor architecture

The executor remains intentionally small.

```text
BaseArbExecutor.sol
EthereumArbExecutor.sol
BscArbExecutor.sol
ArbitrumArbExecutor.sol
OPArbExecutor.sol
```

Shared:

```text
core/
  FlashSourceRouter.sol
  ProfitInvariant.sol
  ExecutionAuth.sol
  RouteValidator.sol

adapters/
  AaveAdapter.sol
  MorphoAdapter.sol
  AerodromeAdapter.sol
  SlipstreamAdapter.sol
  UniswapV3Adapter.sol
  UniswapV4Adapter.sol
  PancakeAdapter.sol
```

Only adapters that have passed the venue admission process are enabled.

## 26.1 No unrestricted calls

Never expose a generic production primitive equivalent to:

```solidity
call(arbitraryTarget, arbitraryCalldata)
```

Use explicit target, selector, pool, token and provider allowlists.

## 26.2 Route validator

Validate before any external call:

```text
chain id
executor address/version
caller authorization
flash provider
venue
pool
input token
output token
route topology
deadline
minimum profit
minimum output
maximum input
hook fingerprint when applicable
```

## 26.3 Multi-asset profit invariant

The v3 single scalar invariant is upgraded.

For each debt asset \(j\):

\[
Balance_{after,j} \ge Debt_j + FlashFee_j + RequiredReturn_j
\]

and for the designated profit asset:

\[
Profit_{realized}\ge MinimumProfit
\]

Any residual non-profit token balance must be explicitly accounted for by route policy; unaccounted residues are a failure.

## 26.4 Residue policy

Valid terminal states are:

```text
all expected debt repaid
required profit received
allowed residue exactly zero
```

or an explicitly declared residue path with deterministic accounting.

## 26.5 Access model

```text
bot execution signer
      ↓
execution contract
      ↓
approved adapters/providers
      ↓
profit recipient
```

Emergency owner/admin is separate from routine execution authority.

---

# 27. Signer, nonce and transaction lifecycle manager

This is promoted to a first-class subsystem.

## 27.1 Signer roles

```text
ExecutionSigner
EmergencyAdmin
Treasury
Observer / read-only
```

No general-purpose wallet should hold unrestricted operational authority.

## 27.2 Nonce manager

For each chain:

```text
confirmed_nonce
pending_nonce
reserved_nonce
submitted_nonce
replacement_set
```

The manager prevents race conditions between strategy workers.

## 27.3 Transaction state machine

```text
CANDIDATE
 → PRECHECK
 → SIMULATED
 → AUTHORIZED
 → SIGNED
 → SUBMITTED
 → SEEN
 → PRECONFIRMED / INCLUDED
 → FINALIZED
 → RECONCILED
```

Failure states:

```text
STALE
REPLACED
REJECTED
REVERTED
DIVERGED
TIMEOUT
```

## 27.4 Replacement policy

Replacement is allowed only while the opportunity's remaining EV exceeds the incremental replacement cost.

No blind gas escalation.

## 27.5 Multi-lane execution signer pool

A single EOA nonce stream is a preventable capture bottleneck. Production therefore uses a measured pool of independent execution signers per chain.

```text
ExecutionSignerPool
├── Lane 0 → nonce manager → executor
├── Lane 1 → nonce manager → executor
├── Lane 2 → nonce manager → executor
└── ...
```

The pool size is driven by observed concurrent high-EV ticket demand rather than a permanent magic number. Hard requirements are:

```text
independent nonce streams
independent pending-state tracking
pre-funded gas reserve
shared immutable executor authorization
per-lane health score
per-lane circuit breaker
no cross-lane nonce reuse
```

The scheduler assigns a ticket to the healthiest currently-free lane with sufficient gas reserve and the required chain/contract authorization.

A signer lane that becomes slow or conflicted is removed from the hot pool without stopping the chain.

---

## 27.6 Signer-pool sizing and rotation

The minimum pool size is determined from the rolling distribution of concurrent deadline-constrained tickets:

\[
N_{signers} \ge Q_{P99}(concurrent\ live\ tickets) + reserve\ margin
\]

with an operational cap set by measured gas-management and security constraints. The pool expands before saturation becomes a capture bottleneck and contracts when sustained utilization falls.

Signer keys are isolated from the general application process as far as latency/security requirements permit. Rotation must preserve contract authorization and cannot create nonce ambiguity. A failed signer is quarantined, its outstanding tickets are reconciled, and only then are its resources returned to the pool.

# 28. Risk engine

Risk is a hard execution gate.

Automatic degradation or halt triggers include:

```text
state staleness
preconf divergence
simulation divergence
revert spike
unexpected callback
venue invariant violation
profit shortfall
fee anomaly
gas anomaly
competitor intensity anomaly
sequencer/builder acceptance collapse
node desynchronization
contract code fingerprint change
flash-source reliability collapse
```

## 28.1 Graduated response

```text
NORMAL
  ↓
REDUCED SIZE
  ↓
HIGH-EV ONLY
  ↓
STRATEGY DISABLED
  ↓
CHAIN DISABLED
  ↓
GLOBAL HALT
```

## 28.2 Loss-event containment

Every loss is classified:

```text
pricing error
state error
simulation error
venue error
inclusion error
fee-model error
contract error
operator/config error
external protocol behavior
```

A loss class that exceeds its expected frequency automatically tightens the gate or disables the affected module.

---

# 29. CPU, RPC and compute economics

The engine has separate resource classes:

```text
state ingestion
state patching
candidate generation
exact pricing
sizing/allocation
simulation
submission
liquidation/event strategies
telemetry
```

## 29.1 Compute opportunity score

For candidate task \(q\):

\[
Priority(q)=\frac{E[Incremental\ NetUSD(q)]}{EstimatedCPU_{ms}(q)+RPCCost(q)}
\]

subject to deadlines.

## 29.2 Budgeting rule

When overloaded:

```text
protect state ingestion
protect exact simulation for high-EV candidates
protect submission
shed exotic searches first
shed low-confidence routes
shed expensive low-hit-rate strategies
```

## 29.3 Admission control

No strategy may create an unbounded queue.

Use:

```text
queue deadline
candidate EV floor
max outstanding simulations
max outstanding RPC calls
per-class CPU budget
```

## 29.4 Capture-critical scheduling policy

Queues are forbidden on the final execution path. Work is admitted only when the system can complete it before its ticket deadline.

Priority order is:

```text
1. already-authorized live tickets
2. high-EV candidates inside capture window
3. exact simulations with high probability of becoming live tickets
4. route discovery
5. slow research / coverage auditing
```

A lower-EV task must never occupy a scarce signer, simulator, or submission lane while a higher-EV ticket is deadline constrained.

## 29.5 Capture-path utilization invariant

For each chain maintain:

\[
U_{capture}=\frac{tickets\ dispatched\ before\ deadline}{tickets\ admitted\ for\ live\ dispatch}
\]

`U_capture` is an infrastructure SLO. Any material decline automatically triggers load shedding, signer-lane expansion, RPC failover, or a reduction in candidate admission thresholds before it becomes a P&L leak.

## 29.6 Capture-assurance control limits

The following are zero-tolerance or hard-budget conditions for the live path:

```text
ticket_drop_count = 0
unexplained_pre_dispatch_expiry = 0
nonce_reuse = 0
wrong_chain_submission = 0
commitment_mismatch = 0
critical_state_gap_used_for_trade = 0
unauthorized_adapter_call = 0
```

Other limits are strategy/chain specific:

```text
max ticket age
max queue residency
max signer-lane utilization
max simulation wait
max feed recovery time
max submission acknowledgement time
max stale-state probability
```

Breaching a hard condition disables new live tickets from the affected path until recovery is verified.

---

# 30. Latency architecture

Measure end-to-end latency as:

\[
T_{signal→submit}=
T_{ingest}+T_{patch}+T_{generate}+T_{price}+T_{size}+T_{simulate}+T_{sign}+T_{submit}
\]

Each component gets p50/p90/p99 telemetry.

## 30.1 Latency optimization priority

A latency improvement is approved only if:

\[
\Delta CaptureEV > \Delta InfrastructureCost + \Delta ComplexityRisk
\]

A faster code path that does not improve realized capture is not a priority.

---

# 31. Node and infrastructure architecture

Recommended production baseline:

```text
high-frequency multi-core CPU
32 GB+ RAM
NVMe
high-quality low-jitter networking
local chain node(s) where economically justified
redundant external RPC
redundant WS feeds
clock synchronization
process isolation
```

For Base specifically, use a Flashblocks-aware feed path and local state reconstruction. The architecture should retain external fallback because Flashblocks is a feed enhancement rather than a guarantee of uninterrupted sub-200 ms service.[^3][^4]

Co-location is preferred only when measured capture improvement justifies cost.

---

# 32. State-of-the-art observability

## State

```text
state_age_ms
flashblock_index
state_patch_ms
state_rebuild_ms
state_branch_count
rollback_count
preconf_final_divergence_rate
state_fingerprint_mismatch
```

## Search

```text
candidates/sec
profitable_candidates/sec
candidate_EV_quantiles
Hermes_rank
full_search_rank
improving_path_miss_rate
finite_size_discovery_rate
```

## Optimization

```text
continuous_size
final_discrete_size
split_ratio
active_pools
single_route_EV
split_route_EV
joint_EV
packed_EV
certificate_status
```

## Execution

```text
signal_to_submit_ms
simulation_ms
signing_ms
submission_ms
earliest_eligible_flashblock
actual_flashblock_index
inclusion_latency
revert_rate
replacement_rate
```

## Capture assurance

```text
tickets_admitted
tickets_dispatched
tickets_acknowledged
tickets_expired_pre_dispatch
ticket_drop_count
dispatch_success_rate
capture_path_utilization
signer_lane_utilization
nonce_conflict_rate
submission_failover_rate
hot_path_recall
high_EV_miss_rate
```

## Competition

```text
capture_rate
competitor_win_rate
arrival_lag
builder_accept_rate
sequencer_accept_rate
bid_to_inclusion_curve
PGA_round_win_rate where applicable
```

## Economics

```text
gross_profit
flash_fee
DEX_fees
L2_execution_fee
L1_data_fee
priority_payment
builder/sequencer payment
failure_cost
net_profit
EV
profit_per_hour
```

## Attribution

Measure incremental P&L from:

```text
single path
parallel splitting
joint allocation
cross-cycle packing
event-driven search
liquidation
correlated strategy
v4 route support
chain-specific submission optimization
compute scheduling
```

---

# 33. Missed-opportunity accounting

Every economically attractive but unexecuted candidate receives a reason code.

```text
LOW_EV
STALE_STATE
TOO_SLOW
COMPETITOR_WON
SIM_FAIL
RISK_FAIL
GAS_FAIL
L1_DATA_COST_FAIL
NO_FLASH_LIQUIDITY
VENUE_DISABLED
CONFLICT_REJECTED
PACKING_NOT_WORTHWHILE
EARLIEST_FLASHBLOCK_TOO_LATE
HOOK_MODEL_INCOMPLETE
BUILDER_REJECTED
SEQUENCER_REJECTED
NONCE_UNAVAILABLE
```

Store:

```text
candidate id
state fingerprint
simulated EV
estimated capture probability
rejection reason
submission policy
later realized outcome where observable
```

The resulting counterfactual dataset becomes a core engineering dataset.

---

# 34. Simulation fidelity and calibration

For every executed trade or credible canary:

```text
gas predicted vs realized
amount out predicted vs realized
profit predicted vs realized
revert classification predicted vs realized
state drift
inclusion outcome
```

Define a strategy/venue simulation fidelity score:

\[
F_{sim}=g(error_{gas},error_{balance},error_{profit},error_{success})
\]

If the score falls outside the configured band:

```text
reduce size
increase simulation tier
disable affected venue/strategy
```

No strategy receives live trust solely because it passed historical backtests.

---

# 35. Differential testing and formal verification

## 35.1 Venue differential tests

For every exact venue engine:

```text
reference implementation
vs
Rust exact model
vs
forked EVM execution
```

Compare:

```text
amounts
fees
state transition
rounding
reverts
```

## 35.2 Fuzzing

Fuzz:

```text
token amounts
tick boundaries
liquidity extremes
fee edges
rounding boundaries
hook return values where legally constrained
adversarial callback ordering
```

## 35.3 Executor invariants

Prove/test:

```text
only authorized caller can execute
only approved provider can callback
only approved venues/pools/tokens can be called
loan always repaid or entire transaction reverts
minimum profit invariant is enforced
unexpected token residue cannot create silent loss
wrong chain cannot execute
expired routes revert
```

Formal verification effort focuses first on the small Solidity execution core, because that is the final capital-control boundary.

---

# 36. Adversarial execution testing

Each strategy class gets a perturbation harness.

Examples:

```text
+ one competing swap
+ larger competing swap
+ opposing swap
+ target state mutation
+ one Flashblock delay
+ two Flashblocks delay
+ gas estimate error
+ L1 fee shock
+ different builder outcome
+ different PGA round outcome
```

A candidate whose profitability disappears under a very small realistic perturbation is treated as fragile and requires a higher margin.

---

# 37. Dynamic opportunity surface

For each:

```text
chain
strategy
venue
route class
hour
volatility regime
state-age bucket
latency bucket
execution mode
```

maintain rolling distributions of:

```text
candidate EV
capture probability
realized net
failure probability
gas
flash premium
competitor intensity
```

The system then dynamically sets:

```text
minimum EV
minimum robustness margin
maximum state age
maximum route complexity
```

---

# 38. Profit target framework

The economic identity is:

\[
MonthlyNet=
\sum RealizedNet
-\sum FailureCosts
-InfrastructureCosts
\]

Target:

\[
\boxed{MonthlyNet > 25,000\ USD}
\]

## 38.1 What proves a credible path

Across multiple market regimes:

```text
sufficient opportunity density
sufficient capture probability
positive realized net P&L
stable simulation fidelity
acceptable failure cost
acceptable infrastructure cost
```

No assumed trade count is used as evidence.

No assumed average profit is used as evidence.

No single historical “hero trade” is evidence.

---

# 39. Production rollout — revised order

## Phase 0 — shadow state and economics

Run on:

```text
Base
Ethereum
BSC
Arbitrum
OP Mainnet
Unichain
```

Enable:

```text
state ingestion
exact pool reconstruction
triangles
short multi-hop
simulation
competition model
chain cost model
```

No live capital.

### Gate

The opportunity surface must be measurably positive after realistic fees and modeled capture.

## Phase 1 — Base core live

Enable:

```text
triangles
short multi-hop
single-path exact sizing
exact simulation
Base Flashblocks-aware submission
```

No cross-cycle packing.

## Phase 2 — BSC and Ethereum economic competition

Enable shadow/live canary comparison based on measured ChainScore.

Do not promote either chain merely because its DEX volume is high.

## Phase 3 — parallel-pool allocation

Enable:

```text
multi-pool split
KKT warm starts
piecewise tick handling
discrete refinement
```

## Phase 4 — programmable venue execution

Enable:

```text
Uniswap V4 exact hooks
custom accounting validation
hook state fingerprints
```

## Phase 5 — joint allocation and bounded packing

Enable:

```text
shared-pool clusters
joint concave allocation
improving-path certificate
conflict graph
bounded packing
```

## Phase 6 — event-driven backruns

Enable:

```text
target-event simulation
post-event route generation
chain-specific inclusion optimization
```

## Phase 7 — liquidation and correlated strategies

Only after the core engine is stable and profitable.

## Phase 8 — adaptive chain portfolio

The system begins automatically reallocating compute and operating resources among chains according to measured realized EV.

---

# 40. Go / no-go gates — final version

A chain/strategy/venue module may enter production only when the applicable conditions are green:

1. State reconstruction matches chain state.
2. Exact AMM math passes differential testing.
3. Exact route simulation matches fork execution.
4. Simulation-vs-realized divergence remains within bounds.
5. Venue adapter passes fuzz/property testing.
6. Flash-loan repayment invariants hold.
7. Contract access controls pass adversarial tests.
8. Revert rate is below class threshold.
9. State staleness is below class threshold.
10. Competitive capture is positive.
11. Realized net P&L is positive after every material cost.
12. At least two materially different market regimes have been observed where feasible.
13. No unexplained loss cluster exceeds the configured threshold.
14. Competitor model is directionally calibrated.
15. Parallel splitting has positive incremental realized P&L before activation.
16. Joint allocation has positive incremental realized P&L before activation.
17. Packing has positive incremental realized P&L after inclusion effects before activation.
18. Liquidation has positive standalone EV after competition.
19. V4 hook-dependent routes have exact or explicitly bounded execution models.
20. Chain-specific fee models are validated.
21. Base earliest-eligible-Flashblock modeling has been empirically validated.
22. Signer/nonce lifecycle passes failure/replacement testing.
23. Circuit breakers trigger safely in controlled tests.
24. Infrastructure expense is justified by incremental captured profit.
25. The measured opportunity surface supports the planned scale.
26. **Capture-assurance invariant:** every admitted live ticket either reaches a network acknowledgement within its dispatch deadline or ends in an explicit failure state; silent expiry is zero-tolerance.
27. **Ticket-drop invariant:** no accepted candidate may be lost between risk approval and dispatch because of internal queue starvation.
28. **Signer-lane health:** no single execution signer is a mandatory system dependency.
29. **Last-mile revalidation:** nonce, critical state, gas eligibility, gas funding and authorization are checked immediately before signing.
30. **Opportunity-coverage audit:** hot-path discovery recall is continuously measured against the independent delayed broad-search oracle.
31. **Capture-path utilization:** infrastructure dispatch utilization remains inside the configured safe band.
32. **USD valuation isolation:** execution decisions remain valid in native/profit-token units; USD conversion is used for portfolio ranking/reporting with conservative bounds and cannot introduce a trade solely because of a manipulable USD mark.
33. **Feed continuity:** no materially incomplete state feed may be used for live execution, and gap recovery is verified before trading resumes.
34. **Submission-stage observability:** transport acceptance, network receipt, ordering/preconfirmation, inclusion and finalization are separately observable.
35. **Capture path saturation:** signer, simulation, RPC and dispatch capacity remain sufficient for the measured concurrent opportunity load.
36. **No silent expiry:** every admitted live ticket reaches a terminal success or explicit failure state with a reason code.

---

# 41. Engineering priority — final order

The architect must execute in this order unless realized P&L data proves a different bottleneck:

```text
1. state correctness
2. speculative/canonical state versioning
3. exact pricing correctness
4. simulation fidelity
5. executable venue coverage
6. chain-specific total-cost modeling
7. Base Flashblock eligibility/scheduling
8. signer/nonce/submission reliability
9. discrete route sizing
10. competitive capture model
11. parallel-pool allocation
12. Uniswap V4 programmable execution
13. joint route allocation
14. shared-pool coupling / improving-path checks
15. adaptive compute allocation
16. Hermes / structure-aware acceleration
17. bounded cross-cycle packing
18. event-driven backruns
19. liquidation
20. correlated strategies
21. gas / calldata micro-optimization
22. exotic research
```

The important change from a feature-driven roadmap is that **submission economics and exact state fidelity are moved ahead of advanced route mathematics** wherever they can directly increase captured dollars.

---

# 42. Things explicitly excluded from production

Remain excluded unless a later research gate proves material incremental value:

```text
JIT liquidity
bridge-dependent arbitrage
cross-chain atomic execution
probabilistic spam
uncommitted transaction probing
public leakage by default
unbounded route search
unbounded nonlinear global optimization
unmodelled V4 hooks
unapproved arbitrary contract calls
```

Cross-chain price data may be used as a signal. Cross-chain bridge/execution settlement is not part of the atomic production arbitrage engine.

---

# 43. Security architecture — final

## Contract level

Mandatory:

```text
reentrancy defense
callback sender verification
role separation
approved provider/venue/pool/token lists
profit invariant
deadline/slippage enforcement
emergency pause
chain-id validation
```

## Adapter level

Every adapter has:

```text
independent configuration
independent deployment record
independent circuit breaker
independent fuzz suite
independent differential suite
independent shadow metrics
```

## Key management

Never combine:

```text
trading signer
admin signer
treasury signer
```

where separation is practical.

## Operational security

Credentials are externalized from source code.

Private keys never enter model logs or telemetry.

RPC credentials, builder credentials and Fast Feed credentials are treated as secrets.

---

# 44. Failure containment

Every external boundary has a bounded failure model:

```text
node unavailable
feed stalled
state divergence
RPC timeout
simulation timeout
builder unavailable
sequencer unavailable
flash source unavailable
venue revert
signing failure
nonce conflict
```

The system must fail **closed** for capital safety and fail **open to alternative providers** where the alternative itself passes health/risk gates.

Example:

```text
Base Flashblocks feed stale
     ↓
stop Flashblocks-dependent candidates
     ↓
continue only routes whose state can be reconstructed safely
     ↓
reduce size / halt according to policy
```

---

# 45. Data schemas — minimum production objects

## Candidate

```text
candidate_id
chain_id
strategy
venue_set
route
state_fingerprint
state_age
flash_source
input_amount
expected_output
gross_profit
dex_fees
flash_fee
total_execution_cost
expected_net_profit
robust_EV
certificate_status
simulation_tier
capture_probability
robustness_margin
deadline
submission_policy
```

## Opportunity outcome

```text
candidate_id
submitted
included
finalized
competitor_observed
actual_state
actual_output
actual_gas
actual_fee
actual_net_profit
failure_code
```

## State version

```text
chain_id
parent_hash
block_number
flashblock_index
state_root
state_delta_hash
observation_time
finality_status
```

## Pool state

```text
pool_id
venue
tokens
exact_model_type
state_version
liquidity/depth
fee_model
hook_metadata
code_fingerprint
last_update
```

---

# 46. Search and execution control plane

APEX-MEV v4 is a deterministic state machine, not a loose collection of workers.

## 46.1 Capture Assurance Controller

The control plane contains a dedicated controller between the risk gate and transaction lifecycle.

```text
EV/RISK GATE
    ↓
OPPORTUNITY TICKET
    ↓
RESOURCE RESERVATION
    ↓
LAST-MILE REVALIDATION
    ↓
SIGNER LANE
    ↓
DISPATCH ROUTER
    ↓
ACK MONITOR
    ↓
PRECONF / INCLUSION MONITOR
    ↓
OUTCOME / EXPLICIT FAILURE
```

The controller owns the invariant that no admitted ticket can disappear silently.

### Mandatory capture protocol

Once a candidate becomes an `AUTHORIZED` ticket, the system must treat it as a deadline-constrained execution asset rather than ordinary work. The capture controller executes the following deterministic protocol:

```text
1. LOCK opportunity commitment
2. RESERVE signer + nonce lane
3. RESERVE simulation / RPC / dispatch capacity
4. REVALIDATE only critical mutable state
5. SIGN exact committed payload
6. DISPATCH through all economically approved lanes in parallel
7. OBSERVE transport / node / sequencer / builder acknowledgement
8. OBSERVE preconfirmation / inclusion
9. IF state changes before inclusion: reprice or explicitly abandon
10. RECONCILE receipt, balances, debt and realized P&L
11. CLOSE ticket with success or explicit failure code
```

The controller has authority to preempt lower-value discovery, simulation and research work to protect an authorized high-EV ticket. FIFO is forbidden on the final capture path.

For the same signed transaction, transport redundancy is permitted. Economically distinct duplicate attempts are permitted only when the submission model proves positive incremental EV and the nonce/replacement policy explicitly accounts for the interaction; otherwise duplicate gas expenditure is prohibited.

The hard capture invariant is:

```text
admitted_live_ticket
    => exactly one terminal lifecycle outcome
       SUCCESS / INCLUDED / FINALIZED
       OR
       EXPLICIT_FAILURE(code, timestamp, state, cause)
```

No internal timeout, worker restart, process crash, queue overflow, signer shortage, RPC error or database failure may create an unclassified outcome. Recovery code must reconcile all in-flight tickets from durable state before new live dispatch is re-enabled.

## 46.2 Parallel execution, not serial architecture

The conceptual state machine remains deterministic, but its internal workers must execute concurrently. For example:

```text
state event
  ├── exact repricing
  ├── finite-size sizing
  ├── competitor scenarios
  └── cost refresh
          ↓
     candidate join
          ↓
   top candidates race
      ├── simulation A
      ├── simulation B
      └── portfolio check
          ↓
       ticketing
          ↓
   signer / dispatch lanes
```

No artificial serialization is permitted where tasks are independent.

## 46.3 Precomputed route frontier

The system keeps a resident frontier of executable route templates for the most active pool neighborhoods:

```text
route topology
venue sequence
token sequence
fee variants
tick neighborhood
hook fingerprint
flash source
expected gas class
```

An event therefore triggers **revaluation of known routes first**, followed by broader discovery. This is the primary mechanism for reducing capture latency without adding another generic graph algorithm.

```text
MARKET EVENT
   ↓
STATE UPDATE
   ↓
DEPENDENCY INVALIDATION
   ↓
CANDIDATE GENERATION
   ↓
ANALYTIC FILTER
   ↓
EXACT PRICING
   ↓
SIZING
   ↓
ALLOCATION
   ↓
COST MODEL
   ↓
SIMULATION
   ↓
ADVERSARIAL CHECK
   ↓
EV / RISK GATE
   ↓
AUTHORIZATION
   ↓
SIGN
   ↓
SUBMIT
   ↓
INCLUSION MONITOR
   ↓
FINAL RECONCILIATION
   ↓
P&L ATTRIBUTION
   ↓
MODEL CALIBRATION
```

Every transition is observable and idempotent.

---

# 47. Adaptive opportunity allocation

The final edge is not any single routing algorithm.

It is the ability to decide **where to spend the next unit of compute, latency budget and transaction capacity**.

For each strategy class \(k\), estimate:

\[
ROI_k=\frac{E[Incremental\ Captured\ NetUSD_k]}{Compute_k+Infra_k+RiskCost_k}
\]

Then allocate resources subject to hard safety constraints.

This allows the system to learn, for example:

```text
“Another 10% CPU on triangles is low value.”
“V4 hook routes are currently high value.”
“BSC has more EV/hour than Base for this regime.”
“Ethereum notional is high but capture probability is poor.”
“Parallel splitting is paying for itself.”
“Packing adds profit gross but loses on inclusion timing.”
```

Those conclusions come from realized evidence, not architecture preference.

---

# 48. Benchmark design

Every major optimization must be evaluated against a frozen baseline.

Examples:

```text
baseline: single-route exact sizing
variant: split allocation

baseline: simple Base submission
variant: Flashblock eligibility optimizer

baseline: route search without Hermes
variant: Hermes-assisted search

baseline: separate candidates
variant: bounded packing

baseline: static chain weights
variant: ChainScore allocator
```

Primary statistic:

\[
\Delta RealizedNetUSD/hour
\]

Secondary:

```text
capture rate
failure rate
compute cost
latency
```

A benchmark result is invalid if the variant has materially different opportunity exposure and the difference is not normalized.

---

# 49. Final architecture diagram

```text
                                      APEX-MEV v4
                                            │
                    ┌───────────────────────┴───────────────────────┐
                    │                                               │
             CHAIN PORTFOLIO                                CAPITAL / COMPUTE
              CONTROLLER                                      ALLOCATION
                    │                                               │
      ┌─────────────┼──────────────┬──────────────┐                 │
      │             │              │              │                 │
    BASE         ETHEREUM         BSC         ARB/OP/UNI            │
      │             │              │              │                 │
      └─────────────┴──────────────┴──────────────┴─────────────────┘
                                    │
                           NORMALIZED STATE BUS
                                    │
                  ┌─────────────────┴─────────────────┐
                  │                                   │
          CANONICAL STATE                     SPECULATIVE STATE TREE
                  │                                   │
                  └─────────────────┬─────────────────┘
                                    │
                         STATE-DIFF / DEP GRAPH
                                    │
          ┌─────────────────────────┼─────────────────────────┐
          │                         │                         │
   NEGATIVE-CYCLE             HERMES / STRUCTURE       EVENT / FINITE-SIZE
      SEARCH                         SEARCH                  SEARCH
          │                         │                         │
          └─────────────────────────┼─────────────────────────┘
                                    │
                            CANDIDATE ROUTES
                                    │
                             EXACT AMM MATH
                                    │
                  ┌─────────────────┼──────────────────┐
                  │                 │                  │
             EXACT SIZING      POOL SPLITTING      MULTI-ROUTE
                  │                 │               ALLOCATION
                  └─────────────────┼──────────────────┘
                                    │
                         SHARED-POOL COUPLING
                                    │
                         DISCRETE WEI REFINEMENT
                                    │
                           CONFLICT GRAPH
                                    │
                     BOUNDED CYCLE PORTFOLIO
                                    │
                          FLASH SOURCE ROUTER
                                    │
                         TOTAL COST ENGINE
                                    │
                      ADVERSARIAL SIMULATION
                                    │
                           EV / ROBUST RISK
                                    │
                   ┌───────────────────┴────────────────────┐
                   │                                        │
          CAPTURE ASSURANCE CONTROLLER             SIGNER / NONCE POOL
                   │                                        │
          TICKET / RESERVE / REVALIDATE              MULTI-LANE EOA
                   │                                        │
                   └───────────────────┬────────────────────┘
                                       │
                              CHAIN EXECUTION ADAPTERS
                                       │
        ┌──────────┬───────────┬──────────┬──────────┐
        │          │           │          │          │
     BASE       ETH PB       BSC      ARB/OP     OTHER
   FLASHBLOCKS  PRIVATE     NATIVE   ACTIVE      VERIFIED
        │          │           │          │          │
        └──────────┴───────────┴──────────┴──────────┘
                              │
                         SOLIDITY CORE
                              │
                   flash → swaps → repay
                              │
                    profit invariant / revert
                              │
                         REALIZED P&L
                              │
                    CALIBRATION / LEARNING
                              │
                    ┌─────────┴─────────┐
                    │                   │
             OPPORTUNITY MODEL     CHAIN SCORE
                    │                   │
                    └─────────┬─────────┘
                              │
                    NEXT RESOURCE DOLLAR
                              │
                    CAPTURE ASSURANCE SLO
```

---

# 50. Final strategic thesis

APEX-MEV v4 does not depend on one “magic” algorithm.

Its defensible architecture is the interaction of:

```text
fresh state
+
versioned speculative execution state
+
incremental dependency propagation
+
negative-cycle / Hermes / finite-size route generation
+
exact AMM mathematics
+
exact finite-size sizing
+
optimal liquidity splitting
+
shared-pool-aware joint allocation
+
discrete refinement
+
competitive latency modeling
+
chain-specific fee modeling
+
chain-specific inclusion optimization
+
adversarial simulation
+
flash-liquidity routing
+
strict Solidity invariants
+
robust risk controls
+
compute economics
+
realized P&L attribution
```

The strongest edge is therefore **systems integration under a single economic objective**.

The system is intentionally not designed to find the most opportunities.

It is designed to find the opportunities most likely to become **realized positive net dollars** and to spend progressively more resources only where the expected marginal return justifies doing so.

---

# 51. Architect implementation mandate

The architect should treat the following as mandatory interfaces, not optional concepts:

```text
StateVersion / StateBranch
ChainExecutionAdapter
VenueAdapter
ExactPricingEngine
FlashSourceRouter
CostModel
Candidate
SimulationResult
CompetitorModel
SubmissionDecision
RiskDecision
ExecutionCommitment
TransactionLifecycle
P&LAttribution
```

Each must be unit-testable and independently observable.

The architecture should be implemented as deterministic modules with explicit inputs/outputs and minimal hidden global state.

---

# 52. Final “next dollar” rule

When two engineering tasks compete, choose the task with the highest measured:

\[
\boxed{
\frac{Expected\ Incremental\ Realized\ NetUSD}{EngineeringCost + OperatingCost + RiskCost}
}
\]

Examples:

- If a 15 ms optimization raises capture on $50 trades by 0.1%, do not prioritize it.
- If correcting a Flashblock eligibility model turns a high-EV route from systematically late to executable, prioritize it.
- If exact V4 hook modeling unlocks a materially large verified route surface, prioritize it.
- If adding another graph algorithm only increases candidate count, do not prioritize it.

**The next engineering dollar goes to the bottleneck that produces the next realized dollar.**

---

# 53. Coverage of APEX-MEV v3 — preserved and upgraded

The following v3 concepts are all retained in v4 unless explicitly marked otherwise:

| v3 component | v4 status | treatment |
|---|---|---|
| Base-first engineering | **Preserved, generalized** | Base remains primary low-latency battlefield; dynamic ChainScore prevents hard-coded economic exclusion of other chains |
| Ethereum secondary | **Preserved** | High-notional private-builder execution domain |
| Triangle arbitrage | **Preserved** | Baseline high-frequency strategy |
| Short multi-hop | **Preserved** | 3–4 hops preferred/allowed by economics |
| Parallel-pool split | **Preserved + upgraded** | Exact shared-pool coupling and gas/L1-cost awareness |
| Event-driven backruns | **Preserved + upgraded** | Target-state simulation and chain-specific timing |
| Liquidations | **Preserved + upgraded** | Protocol-specific eligibility and unwind model |
| Stable/correlated dislocations | **Preserved + upgraded** | External feeds only as signals; on-chain state is truth |
| JIT liquidity | **Excluded** | Remains outside production scope |
| Cross-chain execution | **Excluded** | Remains outside atomic production engine |
| Spam/probing | **Excluded** | No speculative gas burning |
| EV objective | **Upgraded** | Scenario-conditioned instead of naïve independent probability product |
| Incremental graph | **Preserved** | Versioned dependency engine |
| Hermes | **Preserved** | Adaptive accelerator, not authority |
| Exact AMM math | **Preserved** | Expanded for V4 hooks/custom accounting |
| Convex allocation | **Preserved with stricter validity** | Only valid concave/coupled domains; otherwise heuristic/certification disabled |
| Discrete refinement | **Preserved** | Final integer/wei truth |
| Conflict graph | **Preserved** | Extended to Flashblock/call/calldata/signing resource conflicts |
| Adversarial simulation | **Preserved + upgraded** | Scenario tree and empirical competitor model |
| Base Flashblocks | **Preserved + upgraded** | Speculative branch state + gas-limit scheduling |
| Ethereum private execution | **Preserved** | Builder/relay empirical optimization |
| Risk engine | **Preserved** | Hard gate with graduated response |
| Typed Solidity adapters | **Preserved** | Stronger allowlists and commitment validation |
| Profit invariant | **Upgraded** | Multi-asset / residue-aware invariant |
| Key separation | **Preserved** | Signer/admin/treasury separation |
| CPU isolation | **Preserved + upgraded** | Dollar-per-compute resource scheduler |
| Observability | **Preserved + expanded** | State branches, inclusion eligibility, chain economics |
| Missed-opportunity accounting | **Preserved** | Expanded reason taxonomy |
| $25k monthly target | **Preserved** | Measured capacity target, not forecast |
| Phased deployment | **Preserved + reordered** | Earlier chain/cost/submission validation; V4 and packing after core proof |
| Go/no-go gates | **Preserved + strengthened** | Added chain-cost, V4 hook, nonce and Flashblock eligibility gates |

---

# 54. Final production doctrine

The final production doctrine is:

```text
DO
use the freshest trustworthy state
use exact execution mathematics
use finite-size optimization
use empirical inclusion models
use chain-specific fee models
use flash-liquidity routing
use bounded combinatorial allocation
use adversarial simulation
use strict contract invariants
use realized P&L attribution
use dynamic chain/resource allocation

DO NOT
assume graph profitability equals trade profitability
assume DEX volume equals arbitrage EV
assume preconfirmation equals finality
assume gas limit equals gas used
assume v4 behaves like v3
assume path-separable allocation under shared pools
assume inclusion probabilities are independent
assume higher fees always recover timing
assume more routes means more profit
assume historical backtests predict current capture
```

---

# 55. Final statement

APEX-MEV v4 is the final architectural direction for a serious production flash-loan arbitrage/MEV system.

The system is considered successful only when it demonstrates the following chain of evidence:

\[
\boxed{
Measured\ opportunity\ density
+
Measured\ capture\ probability
+
Exact\ simulation\ fidelity
+
Measured\ inclusion\ probability
+
Measured\ realized\ net\ P\&L
}
\]

The $25,000+/month objective is therefore treated correctly: as the output of a machine that must earn it from the market, not as a number produced by an architectural spreadsheet.

**Everything else is a hypothesis until realized.**

---

# 56. Sources and research basis

## Source baseline

1. **APEX-MEV v3 — Final Architect Blueprint**, supplied as `APEX_MEV_v3_Final_Blueprint(1).md`. This file is the architectural baseline. Material v3 concepts are preserved and upgraded in the v3→v4 coverage matrix.

## Current external research

1. **DefiLlama — Base DEX rankings.** Used only as a current market-activity prior; DEX volume is not treated as proof of arbitrage EV.
   - https://defillama.com/dexs/chain/base

2. **DefiLlama — Ethereum DEX rankings.** Used only as a current market-activity prior.
   - https://defillama.com/dexs/chain/ethereum

3. **Base Documentation — Flashblocks API / RPC overview.** Official documentation for pre-confirmed `pending` state, Flashblocks streams, state diffs and related APIs.[^3]
   - https://docs.base.org/base-chain/api-reference/flashblocks-api/flashblocks-api-overview
   - https://docs.base.org/base-chain/api-reference/rpc-overview

4. **Base Blog — Accelerating Base with Flashblocks.** Official description of Flashblock ordering locks and gas-limit capacity constraints.[^4]
   - https://blog.base.dev/accelerating-base-with-flashblocks

5. **Optimism — OP Stack transaction fees.** Official documentation covering execution, L1 data and operator fee components.[^5]
   - https://docs.optimism.io/op-stack/transactions/fees

6. **Uniswap Developers — v4 hooks.** Official documentation establishing pool-specific hooks and hook-controlled lifecycle behavior.[^6]
   - https://developers.uniswap.org/docs/protocols/v4/concepts/hooks

7. **Aave — Flash Loans / address-book resources.** Canonical flash-loan interfaces and deployment-address references.[^7]
   - https://aave.com/docs/developers/flash-loans
   - https://github.com/bgd-labs/aave-address-book

8. **Morpho Documentation — Flash Loans.** Atomic callback/repayment semantics.[^8]
   - https://docs.morpho.org/learn/concepts/flashloans/

9. **Arbitrum Foundation — Constitutional AIP: Transition Arbitrum One ordering policy to PGA.** Current governance specification describing PGA ordering, priority fees, arrival timestamps and capacity behavior.[^9]
   - https://forum.arbitrum.foundation/t/constitutional-aip-transition-arbitrum-one-ordering-policy-to-priority-gas-auctions-pga/30942

10. **Arbitrum Foundation — Constitutional AIP Fast Feed.** Companion governance material for the fast ordering feed.[^10]
   - https://forum.arbitrum.foundation/t/constitutional-aip-fast-feed/31003

11. **Uniswap v3 Core — SqrtPriceMath.** Exact concentrated-liquidity arithmetic and rounding reference.[^11]
   - https://github.com/Uniswap/v3-core/blob/main/contracts/libraries/SqrtPriceMath.sol

12. Farokhnia, Novozhilov, Safaei, Shen, **“Hermes: Scalable and Robust Structure-Aware Optimal Routing for Decentralized Exchanges,” IEEE Blockchain 2025.** Structure-aware routing source.[^12]
   - https://researchportal.hkust.edu.hk/en/publications/hermes-scalable-and-robust-structure-aware-optimal-routing-for-de/
   - https://github.com/SanazSafaei/Hermes-Structure-Aware-Optimal-DEX-Routing

13. Zhavoronkov, **“Multi-Path Routing in Decentralized Exchange Networks: Convex Allocation and an Improving-Path Certificate,” 2026.** Allocation framework source.[^13]
   - https://arxiv.org/abs/2607.22540

14. **Flashbots Docs — JSON-RPC endpoints.** Current private bundle/private-transaction interfaces, builder selection, inclusion windows, cancellation and simulation interfaces.[^14]
   - https://docs.flashbots.net/flashbots-auction/advanced/rpc-endpoint

15. **Base Documentation — `eth_simulateV1`.** Current Flashblocks-aware bundle simulation against pre-confirmed state, with state/block overrides and validation options.[^15]
   - https://docs.base.org/base-chain/api-reference/flashblocks-api/eth_simulateV1

16. **Base Documentation — `base_transactionStatus` and nonce/state APIs.** Current transaction-receipt acknowledgement and pre-confirmed nonce behavior.[^16]
   - https://docs.base.org/base-chain/api-reference/flashblocks-api/base_transactionStatus
   - https://docs.base.org/base-chain/api-reference/ethereum-json-rpc-api/eth_getTransactionCount
   - https://docs.base.org/base-chain/api-reference/ethereum-json-rpc-api/eth_call

17. **Uniswap Developers — v4 Flash Accounting.** Official description of singleton `PoolManager` accounting, deltas, locking and settlement semantics.[^17]
   - https://developers.uniswap.org/docs/protocols/v4/concepts/flash-accounting

## Footnotes

[^1]: DefiLlama, “Base DEX Volume Rankings,” accessed September 12, 2026. https://defillama.com/dexs/chain/base
[^2]: DefiLlama, “Ethereum DEX Volume Rankings,” accessed September 12, 2026. https://defillama.com/dexs/chain/ethereum
[^3]: Base Documentation, “Flashblocks API Overview / RPC Overview.” https://docs.base.org/base-chain/api-reference/flashblocks-api/flashblocks-api-overview ; https://docs.base.org/base-chain/api-reference/rpc-overview
[^4]: Base, “Accelerating Base with Flashblocks.” https://blog.base.dev/accelerating-base-with-flashblocks
[^5]: Optimism Documentation, “OP Stack Transaction Fees.” https://docs.optimism.io/op-stack/transactions/fees
[^6]: Uniswap Developers, “Uniswap v4 Hooks.” https://developers.uniswap.org/docs/protocols/v4/concepts/hooks
[^7]: Aave, “Flash Loans,” and Aave Address Book. https://aave.com/docs/developers/flash-loans ; https://github.com/bgd-labs/aave-address-book
[^8]: Morpho Documentation, “Flash Loans.” https://docs.morpho.org/learn/concepts/flashloans/
[^9]: Arbitrum Foundation, “Constitutional AIP: Transition Arbitrum One ordering policy to Priority Gas Auctions (PGA).” https://forum.arbitrum.foundation/t/constitutional-aip-transition-arbitrum-one-ordering-policy-to-priority-gas-auctions-pga/30942
[^10]: Arbitrum Foundation, “Constitutional AIP Fast Feed.” https://forum.arbitrum.foundation/t/constitutional-aip-fast-feed/31003
[^11]: Uniswap v3 Core, `SqrtPriceMath.sol`. https://github.com/Uniswap/v3-core/blob/main/contracts/libraries/SqrtPriceMath.sol
[^12]: Farokhnia et al., “Hermes: Scalable and Robust Structure-Aware Optimal Routing for Decentralized Exchanges,” IEEE Blockchain 2025. https://researchportal.hkust.edu.hk/en/publications/hermes-scalable-and-robust-structure-aware-optimal-routing-for-de/
[^13]: Zhavoronkov, “Multi-Path Routing in Decentralized Exchange Networks: Convex Allocation and an Improving-Path Certificate,” 2026. https://arxiv.org/abs/2607.22540
[^14]: Flashbots Documentation, “JSON-RPC Endpoints.” https://docs.flashbots.net/flashbots-auction/advanced/rpc-endpoint
[^15]: Base Documentation, “eth_simulateV1.” https://docs.base.org/base-chain/api-reference/flashblocks-api/eth_simulateV1
[^16]: Base Documentation, “base_transactionStatus,” “eth_getTransactionCount,” and “eth_call.” https://docs.base.org/base-chain/api-reference/flashblocks-api/base_transactionStatus ; https://docs.base.org/base-chain/api-reference/ethereum-json-rpc-api/eth_getTransactionCount ; https://docs.base.org/base-chain/api-reference/ethereum-json-rpc-api/eth_call
[^17]: Uniswap Developers, “Uniswap v4 Flash Accounting.” https://developers.uniswap.org/docs/protocols/v4/concepts/flash-accounting


---

# 57. Final capture-assurance protocol — architect acceptance standard

The production architect must implement the following as **non-negotiable control invariants**. They are the final bridge between proven market EV and the system's ability to capture as much of that EV as the external execution market permits.

## 57.1 Mandatory capture protocol

```text
MARKET STATE EVENT
      ↓
STATE VERSION VERIFIED
      ↓
AFFECTED ROUTE FRONTIER REVALUATED
      ↓
FINITE-SIZE EXACT PROFIT
      ↓
EXACT COST / FLASH / GAS / INCLUSION MODEL
      ↓
ADVERSARIAL ROBUSTNESS CHECK
      ↓
OPPORTUNITY TICKET CREATED
      ↓
EXECUTION RESOURCES RESERVED
      ↓
LAST-MILE REVALIDATION
      ↓
SIGNER LANE ASSIGNED
      ↓
SIGNED COMMITMENT
      ↓
PARALLEL ECONOMICALLY-APPROVED DISPATCH
      ↓
TRANSPORT ACK
      ↓
SEQUENCER / BUILDER OBSERVATION
      ↓
PRECONFIRMATION / INCLUSION
      ↓
FINAL RECONCILIATION
      ↓
REALIZED NET P&L
      ↓
CAPTURE + MISS ATTRIBUTION
      ↓
MODEL / RESOURCE RECALIBRATION
```

### 57.1.1 No-loss-of-opportunity invariant

An opportunity admitted to the live path must never be lost because APEX itself failed to execute an available control action. The following are hard failures: queue starvation, signer starvation, nonce collision, stale configuration lookup, missing last-mile revalidation, non-durable ticket state, unobserved submission, unclassified timeout, or avoidable serial blocking.

### 57.1.2 Preemption invariant

When capture capacity becomes scarce, the engine preempts work in this order:

```text
research / coverage
      ↓
slow search
      ↓
low-confidence simulations
      ↓
low-EV candidates
      ↓
high-EV candidates
      ↓
AUTHORIZED LIVE TICKETS — NEVER PREEMPTED
```

### 57.1.3 Capacity invariant

The live system must maintain enough independent signer lanes, simulation capacity, RPC capacity, native-gas reserves and dispatch bandwidth to cover the empirically observed high-EV opportunity concurrency. Capacity is expanded before measured saturation reduces `SYSTEM_CAPTURE_ASSURANCE`.

### 57.1.4 Submission invariant

Submission is never considered complete at RPC acceptance. The ticket remains open until its next externally observable lifecycle state is known. Recovery logic reconciles every outstanding transaction after restart, reconnect, replacement, or provider failure.

### 57.1.5 External-market boundary

No architecture can force an independent sequencer, builder, proposer, chain halt recovery mechanism, or competing searcher to choose APEX's transaction. Accordingly, the absolute engineering guarantee is:

> **APEX guarantees complete, deterministic, deadline-aware execution handling for every opportunity it admits to the live path, with zero silent internal loss. External market capture remains a stochastic outcome and is maximized through latency, state fidelity, submission-path selection, fee/bid optimization, redundancy, and adversarial modeling.**

This distinction is mandatory. Any implementation document claiming deterministic 100% market capture is architecturally unsound.

## 57.2 Final architect sign-off conditions

The architect may declare APEX-MEV v4 production-complete only when:

```text
□ all 36 existing production gates pass
□ capture-assurance invariant has integration-test coverage
□ crash/restart recovery reconciles all live tickets
□ signer/nonce pool failover is tested under load
□ feed-gap recovery blocks unsafe trading and resumes only after verification
□ Base Flashblocks simulation uses the current pre-confirmed state correctly
□ Base gas-limit scheduling is incorporated into earliest-inclusion selection
□ Ethereum private submission lanes are measured independently
□ chain adapters discover and validate their active ordering regime
□ no live-path queue can silently expire an admitted ticket
□ capture-path saturation produces deterministic load shedding
□ every missed opportunity receives a machine-readable reason
□ realized P&L is attributable to route, venue, strategy, chain and optimization layer
□ infrastructure changes are promoted only when incremental captured NetUSD is positive after full costs
```

# 58. Implementation Baseline and ARBOT Migration Doctrine

## 58.1 Mandatory implementation baseline

APEX-MEV v4 SHALL be implemented by transforming the existing `arbot-main2` repository. A blank-repository rewrite is prohibited unless a formally documented technical finding proves that the existing repository is irreconcilably incompatible with the v4 architecture.

The existing repository is **not** the architectural authority. It is a pool of implementation assets whose correctness, compatibility, security and performance must be proven before admission to the v4 production path.

The v4 implementation must preserve the engineering capital already accumulated in `arbot-main2` wherever it remains technically valuable, while preventing legacy orchestration or assumptions from constraining the new architecture.

## 58.2 Authority hierarchy

The implementation authority hierarchy is:

```text
1. APEX-MEV v4 Final Architect Blueprint
2. APEX-MEV v4 PLAN.md
3. Existing arbot-main2 implementation
4. Existing ARBOT plans, documentation and historical assumptions
```

If existing implementation conflicts with the blueprint, the blueprint wins. If `PLAN.md` incorrectly translates the blueprint, `PLAN.md` must be corrected before implementation proceeds. Existing code, documentation and historical assumptions never silently override the v4 architecture.

## 58.3 Mandatory component classification

Every material existing subsystem, module, contract, adapter, execution path and operational component must be classified exactly once:

```text
KEEP
ADAPT
REBUILD
REMOVE
UNKNOWN
```

### KEEP

The component is correct, tested, performant, secure and structurally compatible with v4. It may become part of the production architecture without material architectural compromise.

### ADAPT

The underlying capability is valuable and sufficiently correct, but its interfaces, ownership model, data contracts or orchestration must change to satisfy v4.

### REBUILD

The capability is required by v4, but the existing implementation cannot satisfy the required correctness, latency, state, economics, security or reliability properties without replacing its implementation. Useful mathematical or technical primitives should still be preserved where practical.

### REMOVE

The component is obsolete, duplicated, unsafe, economically inferior, or architecturally contradictory. All dependants must be identified before removal.

### UNKNOWN

The component has insufficient evidence of correctness or compatibility and is prohibited from the production execution path until verified.

## 58.4 Repository archaeology is a mandatory first phase

The first implementation phase is **repository archaeology and migration mapping, not coding**. Before substantive implementation begins, the engineering system must establish:

```text
current repository
      ↓
component inventory
      ↓
KEEP / ADAPT / REBUILD / REMOVE / UNKNOWN
      ↓
blueprint requirements mapping
      ↓
dependency graph
      ↓
target architecture
      ↓
implementation sequence
```

This phase must expose hidden coupling, obsolete assumptions, duplicated functionality, unsafe legacy execution paths, shared mutable state, serial bottlenecks, stale chain assumptions, and components whose apparent correctness has not been independently demonstrated.

## 58.5 Preserve proven implementation assets

Where technically justified, the migration should retain proven assets from `arbot-main2`, including but not limited to:

```text
exact AMM / concentrated-liquidity mathematics
tick processing
simulation primitives
pool-state reconstruction
ABI handling
chain/RPC primitives
Base fast-path infrastructure
accounting primitives
Solidity test fixtures
Foundry infrastructure
deployment tooling
operational tooling
Prometheus / telemetry infrastructure
known-good venue adapters
production failure knowledge encoded in tests and diagnostics
```

Retention is conditional on v4 verification. Historical familiarity is not evidence of correctness.

## 58.6 Legacy orchestration must not survive by inertia

Existing ARBOT orchestration may be retained only where it satisfies the v4 architecture. Legacy behaviour must not survive merely because it is already implemented. In particular, any obsolete:

- execution lifecycle;
- chain-priority assumption;
- state ownership model;
- public submission path;
- serial execution bottleneck;
- economic guard;
- stale configuration assumption;
- route/search orchestration;
- resource scheduler;
- accounting model;

must be explicitly classified and either adapted or replaced.

## 58.7 Controlled migration and differential verification

Where replacement of a proven component creates material execution risk, the migration should use a controlled parallel path:

```text
existing implementation
        +
new v4 implementation
        ↓
differential comparison
        ↓
verification
        ↓
traffic migration
        ↓
legacy retirement
```

This pattern is preferred for high-consequence components such as:

```text
exact pricing
state reconstruction
simulation
transaction encoding
accounting
execution settlement
```

The old implementation may only be retired after the replacement satisfies the applicable v4 correctness, performance, security and production gates.

## 58.8 Git and rollback doctrine

The migration must preserve Git history and must not use a destructive repository reset merely to obtain a clean architecture.

Before deleting or materially replacing a subsystem:

```text
identify dependants
→ establish replacement
→ differential-test
→ migrate traffic
→ verify production gates
→ retain rollback path
→ remove obsolete component
```

The migration architecture must support rollback until the replacement has demonstrated correctness in the relevant live or canary environment.

## 58.9 Blueprint-to-repository traceability

Every material v4 requirement must map to:

```text
blueprint requirement
        ↓
PLAN task
        ↓
repository component(s)
        ↓
file(s)
        ↓
test(s)
        ↓
acceptance gate
```

No material blueprint requirement may remain unassigned. Conversely, repository components without a defensible role in the v4 architecture should be removed, isolated, or explicitly retained only as test/reference assets.

## 58.10 Migration success condition

The desired final state is:

```text
PROVEN ARBOT IMPLEMENTATION ASSETS
                +
APEX-MEV v4 ARCHITECTURE
                ↓
      APEX-MEV v4 PRODUCTION SYSTEM
```

It is explicitly **not**:

```text
ARBot + random v4 features
```

and it is explicitly **not**:

```text
blank repository + recreated functionality
```

The migration is successful only when APEX-MEV v4 has clean architectural ownership of the system while retaining every existing implementation asset whose measured value justifies its continued existence.

# 59. Final architectural disposition

**APEX-MEV v4 is the final production blueprint.** The architecture is optimized around one objective: maximize realized net USD captured per unit time, subject to exact execution correctness and bounded failure risk.

The market may offer EV that no software can force a sequencer or builder to award. The system's job is therefore to remove every avoidable internal source of lost capture and to make every remaining external miss observable, measurable, explainable and economically actionable.

The governing equation is:

\[
\boxed{CapturedNetUSD = AvailableMarketEV \times DiscoveryRecall \times ExecutionReadiness \times ExternalCaptureProbability - TotalExecutionCost}
\]

The first three terms are engineering-controlled. The fourth is competitive and must be optimized continuously. The final system is complete only when those terms are measured at production scale and the resource allocator routes the next compute, latency and infrastructure dollar to the highest incremental captured NetUSD.
