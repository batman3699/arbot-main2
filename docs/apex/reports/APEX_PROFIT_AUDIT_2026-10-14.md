# APEX-MEV / arbot-main2 — Profit-Line Audit

**Prepared:** 14 October 2026  
**Reviewed:** main at 27c839c0c2f7238f717ff2d6d684dcdc0df9b85e  
**Audience:** Protocol developer, Rust/runtime engineer, quant/economics owner, operator  
**Objective:** Maximize realized net profit—not route counts, gross quote, or shadow-estimated EV.

## Executive decision

This is now a sophisticated Base opportunity-detection and execution framework: it has coherent versioned pool state, exact two-hop sizing, concentrated-liquidity tick-crossing quotes, per-route gas modeling, lender-aware bounds, simulation, committed calldata, risk gates, a Solidity profit invariant, race/flashblock ledgers, and an armed-backrun shadow evaluator.

**Profitability is not yet demonstrated by the reviewed operating mode.** The co-located host runbook says the shadow uses a null dispatcher and never broadcasts; ops/shadow.base.yaml sets armed.mode to shadow. A simulated quote-to-call match proves useful execution correctness, but does not prove that the bot wins the auction, lands the transaction, or earns net profit after actual costs. The first priority is to pass the already-specified shadow gate and run a tightly capped live pilot—not to add features.

### Ranked actions

1. **Complete the evidence-gated armed-live pilot.** Require the existing Gate A: at least 3 days of armed-shadow, at least 30 fires, positive pessimistic net per day for one bid fraction, and at least 95% quote/simulation agreement on checked fires. Then operator-authorize a capped pilot with separate keys, max size/priority fee, kill switch, and the specified US$25 UTC-day loss cap. Do not bypass the gates.
2. **Resolve a likely over-conservative failure-cost treatment in scenario EV.** The economics model adds an expected failure reserve, then the scenario model separately includes a venue-revert branch. This may reject profitable trades by applying success cost in the revert world while also carrying the expected failure reserve. Add the hand-calculated test below before changing policy.
3. **Recover reactive-lane latency lost to the wider sizing range.** Repository notes report receive-to-proposed median moving from 40 ms (R26.3, n=167) to 62 ms (R27, n=12), then 52 ms (next build, n=3). The last samples are small; the documented culprit is the 4,400× wider size range and about 128 quote evaluations per search after Morpho support.
4. **Calibrate EV and priority-bid models from race/flashblock/armed/receipt data.** Scenario priors are marked unmeasured, and the reactive lane models priority fee as zero because the value of bidding has not yet been measured.
5. **Reduce value-weighted armed-slot staleness.** R27.1 improved current-state pricing to 95% of 316 slots, leaving 19 waiting. The target is not a prettier slot-count metric: it is lower missed opportunity EV without using stale prices.
6. **Close the cost/settlement loop.** Validate the L1-data fee estimate against exact encoded transaction bytes and receipts, synchronize unmeasured admission gas metadata with the measured settlement estimator, and reconcile all landed/reverted transactions to realized token net.

Do **not** prioritize deeper routes or more networks until the current Base two-hop WETH pipeline has measured, receipt-based net profitability. Any expansion must prove incremental realized net, not merely more candidate routes.

## Scope and evidence

I inspected the reviewed main tree and recent commits, focusing on runtime economics and sizing, the live pool book, venue admission/quotes, settlement gas, flash-loan selection, simulation and last-mile validation, risk/eligibility, armed backruns, deployment/configuration notes, P&L attribution and missed-opportunity telemetry.

The latest GitHub Actions CI run for the reviewed head passed: https://github.com/batman3699/arbot-main2/actions/runs/38455927039. Project notes still identify two known-red scripts—scripts/ci/check_placeholder_endpoints.sh and scripts/ci/cl_parity_sweep.sh—so the passing CI run must not be described as every documented gate passing.

This is a repository/evidence audit, not a reproduction of the production host. I did not read the host's live ledgers, secrets, RPC endpoints, receipts, or AWS metrics. Empirical figures below are figures recorded in the repository and should be reproduced against current host state before raising risk limits.

## How the profit line works

The full path is: event → pool-state reconstruction → route discovery → exact size search → net economics/eligibility → simulation → committed call and last-mile revalidation → auction/capture → receipt reconciliation.

Relevant code:
- State and triggers: crates/apex-runtime/src/live/feed.rs, book.rs, frontier.rs.
- Pricing, gas and lenders: crates/apex-runtime/src/live/pricing.rs, gas.rs, lenders.rs; crates/apex-econ/src/sizing/.
- EV/risk: crates/apex-runtime/src/econ.rs, risk.rs; crates/apex-econ/src/ev/scenario.rs and eligibility.rs.
- Call/simulation/control plane: crates/apex-runtime/src/live/calls.rs, sim.rs and plane.rs.
- Armed capture: crates/apex-runtime/src/armed/ and crates/apex-runtime/src/shadow/race.rs.
- Realized attribution: crates/apex-obs/src/pnl.rs and coverage.rs.

Today the operating config stops before real market capture: it uses shadow/null dispatch. The main business question is therefore not simply whether a spread exists; it is which positive-net opportunities the system can win, execute, and reconcile after all costs.

## Findings ranked by expected profit impact

### P0 — No demonstrated realized P&L from the current operating mode

**Evidence:** ops/host/README.md explicitly says the co-located process does not broadcast and uses the null dispatcher. ops/shadow.base.yaml sets armed.mode to shadow. The armed-backruns specification makes live operation a separately gated pilot.

**Impact:** The framework can find and simulate opportunities, but the reviewed run configuration cannot turn those opportunities into realized trading revenue. Shadow estimated net and simulation gross are not realized P&L.

**Action:** Advance only through the existing Gate A. Start with separate armed sender keys, explicit max_priority_gwei, max_bid_fraction, max_size_weth and max_arms, the kill file, a daily loss cap no greater than US$25 per UTC day, and the API/nonce probe. Keep shadow and live processes from sharing sender nonces. Do not skip simulation or last-mile safety gates.

**Acceptance:** Every actual landing or revert reconciles transaction hash, route/rung, lender, amount, ProfitRealised gross/owner fee, gasUsed × effectiveGasPrice, Base L1 fee, priority fee/bid, residual token deltas, realized token net and conservative USD bounds. Shadow rows do not satisfy this gate.

### P1 — Scenario EV may apply failure cost inconsistently and suppress profitable candidates

**Code path:** In crates/apex-runtime/src/econ.rs, LiveEconomics::total_cost() includes expected_failure_cost in TotalExecutionCost, and cost_i128() sums it. LiveEconomics::scenarios() then passes that total into ScenarioPriors::scenario_set(gross, cost). scenario_set charges gross minus cost in the success, delayed and competitor worlds, and minus cost again in the VenueRevert world. crates/apex-econ/src/cost/failure.rs defines expected_failure_cost as probability-weighted failure cost, while the scenario set independently assigns probability to a venue-revert outcome.

**Why it matters:** The revert world still uses a cost basis that includes successful-execution gas, while the same basis carries a probability-weighted failure reserve. If the scenario's revert probability and FailureProfile.failure_ppm describe the same event, this is inconsistent with conditional branch-cost EV and can reject trades that would be positive EV under a correctly conditioned model. The configuration's default rates are both 5%, increasing the importance of making the relationship explicit.

This is a high-confidence accounting concern, not evidence that the bot is losing. The conservative bias can reduce capture by rejecting marginal positive-EV trades; prove the math before changing thresholds.

**Action/test:** Add a deterministic hand-calculated test with a success branch and a revert branch. Assert engine EV equals:

P(success) × (gross − success_cost) + P(revert) × (−failure_cost) − unconditional_irrecoverable_cost.

Use one authoritative probability for this failure event. Make it impossible for an expected failure reserve to be included in every branch and also represented by a conditional revert branch. If an additional conservative margin is desired, expose it separately as a risk buffer rather than calling it expected failure cost.

**Acceptance:** Tests agree with hand calculation across normal/edge cases; a recorded candidate replay compares current EV with conditional EV and reports how many net-positive candidates change eligibility. Do not weaken freshness, simulation, authorization, liquidity or loss controls.

### P1 — Exact sizing is a live latency bottleneck

**Evidence:** PLAN.md Task 8.5 R27/R27.1 records the expanded size search as roughly 4,400× wider following Morpho support and about 128 quote evaluations per search. Receive-to-proposed median was 40 ms in R26.3 (167 candidates), 62 ms in R27 (12 candidates), and 52 ms on the next build (3 candidates). The later sample sizes are small, but the documented cause is the broader size range. LiveEconomics::evaluate also repeats the same sizing evaluation through multiple independent economics-port methods; the code notes this trade-off has not been benchmarked.

**Impact:** On Base, a delay during pricing, simulation and submission can forfeit an opportunity to a rival. Bigger loan capacity is useful only if the system can search, simulate and compete in time.

**Action:** Build a reproducible benchmark over recorded pool states, sizes from small trades through 80+ WETH, and low/high tick crossings. Use the continuous warm start to establish a bracket; do bounded coarse/logarithmic probes to bracket a wide range, then exact integer refinement locally. Do not assume the profit objective is smooth through tick-crossing gas steps. Track evaluations and latency by phase. Consider reusing a pure, immutable, state-fingerprinted evaluation witness across stages only if dependency and concurrency invariants remain intact. Exhausted budgets must be explicit missed opportunities, not silent drops.

**Acceptance:** Report p50/p95/p99 end-to-end candidate latency, quote-evaluation count, size regret against a slow exact oracle, and replayed net capture. No increase in false-positive quotes or simulation mismatches.

### P1 — EV priors and reactive bidding are not market-calibrated

**Evidence:** Default ScenarioPriors in crates/apex-runtime/src/econ.rs are explicitly marked Unmeasured. They assign 250k ppm to same-state immediate, 350k to one-flashblock delay, 350k to competitor-first and 50k to venue revert. LiveEconomics::total_cost sets priority_fee to zero because the value of a bid was not measured. The race/flashblock ledgers and armed evaluator already record rival timing, competing transactions, priority fees and possible net at alternative bid fractions.

**Impact:** Fixed priors can reject good states or accept bad ones; a zero-bid reactive submission can lose an auction even if its price is correct; overbidding can donate the edge to the auction.

**Action:** Estimate empirical win/survival/cost curves from race, flashblock, armed-verdict and eventual receipt data. Segment by route/venue, trigger size, edge, fee regime, trigger-to-submit latency and transaction size. Measure same-state survival, inclusion/success/revert rates, competitor effective priority fee, gross decay, and realized costs. Begin with stratified empirical curves and confidence intervals, not a large ML model. Use time-ordered train/holdout splits and optimize pessimistic expected net at bid b—not win rate. Keep priors marked unmeasured until validated.

**Acceptance:** The chosen reactive bid policy beats zero-bid and fixed-bid baselines on holdout data after all costs. Every bid is capped by explicit priority-fee and net-margin limits.

### P1 — Morpho expands size capacity; benefit is not yet proven end-to-end

**Evidence:** crates/apex-runtime/src/live/lenders.rs selects Balancer while its balance covers the amount, and Morpho above that. Recorded balances have been around 18.7–19 WETH for Balancer and 82k+ WETH for Morpho. PLAN.md says Balancer limited 61% of +20 rungs, 98% of +50, 95% of +100 and all +200 rungs.

The Morpho lender was proven with the real executor in eth_simulateV1; a recorded 20 WETH case produced gross 13,005,938,766,390,153 wei matching the quote with full repayment. Extra gas is state-dependent: approximately 121,700 gas if lender/executor have no WETH dust, versus approximately 82,900 gas with 1 wei held by each; code/config target 122,000 or 83,000. At last documented host check, 328 of 812 arms used Morpho, but no fire had yet been observed, so the evaluator had not judged a Morpho-funded fire.

**Impact:** More capacity unlocks size and spike rungs, but does not guarantee more net. Morpho adds gas; that also increases the bid cost per unit of auction share. Larger size may cross more ticks and shift price impact.

**Action:** Keep Balancer when it covers the size. Choose Morpho only when the larger reachable net surplus beats added gas plus bid and failure risk. Price each rung with the chosen lender's real gas overhead. Monitor actual balances and warn if the expected WETH dust state does not hold.

**Acceptance:** Compare counterfactual best net with Balancer-only cap against the actual size/lender choice for fired events, including gas, L1, bid, failures and realized net. Armed counts alone are not proof.

### P1 — Armed repricing is improved but leaves unpriced current-state slots

**Evidence:** PLAN.md records an improvement from 82% to 95% of 316 slots current at the observed book; stale waiting fell from 72 to 19, with about 9.2 passes/s and roughly 75 ms/pass. armed/engine.rs prioritizes changed slots and only allows current-version rungs to be ranked, armed or renewed.

**Impact:** The freshness design is correct, but a slot that remains stale cannot safely capture its opportunity. Equal slot counts obscure whether the stale set contains high-value or just low-value opportunities.

**Action:** Keep changed pools first, then rank work by expected net, recent trigger activity, move frequency and opportunity decay. Add EV-weighted stale exposure to status. Benchmark fewer low-value rungs and faster exact sizing before blindly adding workers. Do not arm stale prices to improve coverage percentages.

**Acceptance:** Replay the current scheduler versus alternatives, reporting stale-time distribution and positive-net EV captured. ≥99% current slots during active periods is a useful initial engineering target; pass/fail must ultimately depend on lower EV-weighted stale exposure and no correctness regression.

### P2 — Fee and gas provenance should match economic precision

**Evidence:** econ.rs prices Base Fjord L1 data cost using CompressedSize::Estimated and a per-hop calldata estimate (132 bytes overhead + 196 bytes/hop), not the exact encoded/compressed transaction payload. live/admission.rs sets GAS_PER_HOP=267,050 as half of an older 534,100-gas two-hop run, and marks it unmeasured. The dedicated live/gas.rs path does have route/tick-specific measurements. The config's max_cost_confidence_bps=10,000 is explicitly for shadow measurement; the default is 1,000 bps for live.

**Action:** Estimate compression cost from the exact encoded payload before final admission and compare it with actual receipt L1 fees. Track forecast residual distribution. Make admission/commitment gas provenance reflect the settlement estimator or label it advisory. Recalibrate failure rates from receipts; no sample is not a zero failure rate. Keep live cost-confidence limits conservative until evidence supports a change.

**Acceptance:** Report p50/p90/p99 predicted-versus-realized gas/L1/failure cost. Require positive net at the declared confidence bound. Do not copy the shadow's loosened uncertainty cap into live mode.

### P2 — Route expansion must be EV-weighted, not count-driven

**Evidence:** live/frontier.rs deliberately builds two-hop WETH cycles; its comments say tested three-/four-hop candidates were worse in the available census, and token-priced USD conversion is absent from admission economics. inventory.rs and ops/shadow.base.yaml describe a multi-venue Base universe with 3,000 ppm fee ceiling and $100k external-depth discovery filter. apex-obs/coverage.rs already compares against an independent broader search and measures missed value by EV rather than count.

**Action:** Keep two-hop WETH cycles as baseline. Use the broad-search oracle to measure missed positive net EV by hop class and venue. Add depth, non-WETH numeraires or more venues only when replay demonstrates incremental opportunities after exact venue fees, gas, lending and bidding. New venue onboarding requires code/factory validation, state reconstruction, quote parity, measured gas/revert classification, adapter tests and simulation parity.

**Acceptance:** Out-of-sample incremental executable net EV per hour; no duplicated P&L credit. Disable a class if its incremental value cannot be demonstrated.

### P2 — Update deployed-state truth and expose ledger drops

**Evidence:** docs/apex/DEPLOYED.md is a 3 October snapshot and says the executor/router hold nothing, while the R27 plan records Morpho deployment and WETH dust seeding on 13 October. Both statements may be correct at their own timestamps, but the active operational pointer is stale. shadow/race.rs uses nonblocking try_send and exposes a dropped counter, which protects the capture hot path but can bias the dataset if dropped lines are ignored.

**Action:** Update the deployed-state document or link an authoritative current state with observation block/time, executor and lender addresses/code hashes, balances/dust status, broadcasting mode and active caps. Show the race-ledger dropped count and alert if nonzero. Exclude or qualify missing-evidence windows when fitting the auction model; persist raw verdicts and receipt rows with revision/config hash.

**Acceptance:** One operator view says which contracts/lenders are used, whether any process can broadcast, which caps are active, and what share of evidence was dropped. No P&L window is considered complete while its landing/outcome evidence is missing.

## Three-stage implementation plan

### Stage A — Make measurements trustworthy

1. Add the conditional failure-cost EV test; compare output against hand calculation.
2. Benchmark exact size search on recorded states and capture count, candidate latency, stale rejection and net estimate.
3. Verify deployed addresses/hashes/balances against chain; update current deployment documentation and health checks for Morpho dust/gas.
4. Emit one comparable per-candidate record: route, pool versions, timing, amount, gross, gas distribution, L1 estimate/provenance, EV/prior version, simulation result, lender, bid assumption and decline reason.

**Exit:** economics tests pass; known CI-red gates are understood and classified; no silent missing-data path; exact quotes and simulation match on fixtures.

### Stage B — Convert shadow value into capped landings

Use the existing armed-backrun Gate A without shortcuts: ≥3 days armed-shadow, ≥30 fires, positive pessimistic net/day for some bid fraction, and ≥95% agreement for checked quote/simulation pairs. Operator approval is required before authorizing/funding separate sender keys.

Run the pilot with explicit max priority fee, bid fraction and size, max arms, kill file, and no more than the specified US$25 per UTC-day loss cap. Probe sequencer acceptance, false/true predicates, replacement and nonce reuse with one sender. Gate B requires seven live days with positive realized net after every cost, included-revert fees below 20% of gross, and zero tripwire events. If it fails, return to shadow and diagnose—do not scale.

### Stage C — Improve net capture per unit time

1. Calibrate outcome and auction curves out of sample.
2. Speed exact size selection and reduce redundant evaluation.
3. Reduce EV-weighted stale armed coverage.
4. Optimize lender and priority bid jointly on net profit.
5. Add route/venue coverage only if the independent oracle demonstrates incremental value.
6. Increase sender count and caps only after Gate B.

## Metrics that should drive engineering priorities

The canonical objective is **realized net USD per hour**, with token-denominated net authoritative and USD represented as a conservative range. Select the next engineering task by expected incremental realized net divided by engineering time, operating cost and risk.

| Layer | Required measures | Decision |
|---|---|---|
| Discovery | event/candidate count, route class, oracle missed positive net EV | Is coverage limiting value? |
| State | version gaps, state age, tick-ladder refusals, quote mismatch by venue | Is stale or inaccurate state costing trades? |
| Sizing | evaluations, p50/p95/p99 latency, chosen size vs exact oracle, miss reason | Does wider size capacity sacrifice capture? |
| Economics | success cost, failure cost, L1 residual, cost confidence, prior version | Are we overrejecting or mispricing risk? |
| Capture | trigger-to-proposal, sim/sign/ack latency, rival offset and fee | Is speed or bid profitable? |
| Armed | current slots, stale waiting, EV-weighted stale exposure, fires and pessimistic bid net | Would more compute/arms buy value? |
| Execution | simulation/receipt agreement, gas, L1 fee, priority fee, included reverts, lender | Does forecast match settlement? |
| P&L | realized gross, all realized costs, token net, USD bounds, incremental layer P&L | Does a change actually create value? |

Optimization-layer attribution is overlapping, not additive. Use counterfactual fields such as single-route versus split-route EV to measure incremental contribution. Never sum venue or optimization-layer subtotals as if they partition total realized profit.

## Required pre-scale tests

1. **EV/accounting:** conditional success/revert EV matches hand calculation; failure probability and included-revert L1 fee counted once.
2. **Sizing:** fast search is tested against a slow exact oracle over recorded and generated pool states; quantify regret and budget exhaustion.
3. **Quotes/state:** per-venue parity at the same block, including dynamic Slipstream fees, Aerodrome v2 rounding and tick crossings.
4. **Freshness:** a rung cannot arm or submit using pool versions different from those used for its threshold, mid amount and gas.
5. **Lender:** boundary around actual Balancer/Morpho balances, both dust states, bytecode/hash, full repayment, no Aave path.
6. **Settlement:** every receipt reconciles token deltas and every fee; unexplained residue, nonce change, commitment mismatch or unauthorized call blocks scale-up.
7. **Auction:** time-ordered holdout proves pessimistic positive net after bid/revert cost; winning rate alone is insufficient.
8. **Operations:** kill-file and daily-loss-cap drills, sender/nonce separation, crash recovery, RPC quota alerts, ledger-drop alerts, secrets never logged.

## Strengths to preserve

- Coherent block-pinned state, versioned reads, ordered logs and fail-closed tick-ladder limits.
- Exact integer size refinement after a floating-point warm start, with bounded evaluations and distinct unquotable/no-profitable-size outcomes.
- Real token balances and lender holdings bound trade size; concentrated-liquidity virtual reserves are not used as the sole capital bound.
- Route/tick-specific gas modeling rather than a single 534k-gas guess.
- Sealed venue admission with code/factory/token/fee evidence; unknown or non-standard token transfers are not treated as exact.
- Commitment-bound call construction, last-mile checks, tiered simulation and Solidity ProfitInvariant repayment/residue checks.
- Value-weighted coverage and missed-opportunity ledgers; incremental P&L attribution that distinguishes partitions from overlapping dimensions.

## Final instruction to the engineering team

**Do not start with a feature expansion.** Prove the economics accounting, reduce the newly exposed sizing latency, calibrate how opportunities survive and win, then pass the existing gated armed-live pilot. Every change must demonstrate incremental value against a recorded counterfactual and holdout data. Judge it on realized net after gas, Base L1 data fee, bids, included reverts and lender behavior.

As of the reviewed revision, APEX has promising execution and capture infrastructure, but profitability remains unproven until receipt-reconciled live results establish it.

## Primary references

- Current reviewed head: https://github.com/batman3699/arbot-main2/commit/27c839c0c2f7238f717ff2d6d684dcdc0df9b85e
- PLAN.md performance / R27.1 notes: https://github.com/batman3699/arbot-main2/blob/main/PLAN.md
- Armed-backruns design and Gate A/Gate B: https://github.com/batman3699/arbot-main2/blob/main/docs/superpowers/specs/2026-10-13-armed-backruns-design.md
- Morpho flash-lender design: https://github.com/batman3699/arbot-main2/blob/main/docs/superpowers/specs/2026-10-13-morpho-flash-lender-design.md
- Runtime economics: https://github.com/batman3699/arbot-main2/blob/main/crates/apex-runtime/src/econ.rs
- Live pricing: https://github.com/batman3699/arbot-main2/blob/main/crates/apex-runtime/src/live/pricing.rs
- Live gas and lenders: https://github.com/batman3699/arbot-main2/blob/main/crates/apex-runtime/src/live/gas.rs and https://github.com/batman3699/arbot-main2/blob/main/crates/apex-runtime/src/live/lenders.rs
- P&L attribution and coverage oracle: https://github.com/batman3699/arbot-main2/blob/main/crates/apex-obs/src/pnl.rs and https://github.com/batman3699/arbot-main2/blob/main/crates/apex-obs/src/coverage.rs


---

## 10. Live AWS shadow telemetry addendum — 14 October 2026

**Why this update matters:** The initial audit primarily reviewed code, runbooks, design gates, and previously recorded performance results. This addendum incorporates a read-only inspection of the running AWS shadow host on 14 October 2026 (approximately 22:44 UTC on 13 October / 11:44 NZDT on 14 October). These are live-host observations, not a claim that the system is executing live trades.

**Method and boundaries:** The connected AWS integration queried the EC2/SSM control plane and used AWS Systems Manager to run read-only inspection and aggregation commands against the host. The apex-shadow.service was active. The inspection did not edit files, change services, read environment files, access private keys, or enable dispatch. The data snapshots were taken at slightly different moments while the service continued running, so small counter differences between the latest report and JSONL files are expected.

### 10.1 What the host actually reported

| Signal | Observed result | Interpretation |
|---|---:|---|
| Armed lifecycle records | 29,098 (16,350 arm, 12,748 renew) | The armed engine is actively creating and refreshing candidates. |
| Armed-verdict records | 16,021 | Large evaluated sample, but it is not a count of profitable opportunities. |
| Verdicts marked fired=true | 0 / 16,021 | No shadow verdict in this file recorded a fire. |
| Verdicts marked trigger_met=true | 5 / 16,021 | Five trigger conditions were reported as met; this alone does not mean an executable profitable trade existed. |
| Trigger-met verdicts with populated sim, gross_wei, or per_f | 0 | The downstream evidence needed to judge these cases is absent from these verdict records. |
| Flashblock ledger | 90,213 records spanning 8,203 unique block numbers in the JSONL snapshot | High-volume capture is operating; record count is not equivalent to distinct opportunities. |
| Shadow report funnel | 19,060 events; 94 skipped as unverified; zero redelivered or suppressed; one LOW_EV/NoProfitableSize decline | Capture is functioning, but the current report does not show a meaningful completed profitable-trade sample. |
| Funnel closures | Five failed/Reverted; one shadow_dispatched/in_time; one LOW_EV/NoProfitableSize decline | These are shadow pipeline classifications. Do **not** describe the five as confirmed on-chain transaction reverts unless a separate chain receipt proves that. |
| Null dispatch / settlement | null_dispatched=1; settlement_asked=0 | Consistent with the host running in shadow mode rather than broadcasting transactions. |
| Capture / feed integrity | capture_assurance=1.0; 19,060 published; zero dropped | Positive evidence for the feed's measured capture path, not proof of execution profitability. |
| Flashblock decoding | Zero decode failures, incomplete blocks, or ledger failures in the latest report | No obvious ingestion-quality failure in these counters. |
| Armed engine freshness | 316 slots; 316 current; zero stale waiting; last tick 2,876 μs | Slot freshness looks healthy in this snapshot. |
| Universe admitted to the live book | 41 held/admitted pools, five venues, book status Verified | More venue coverage is not the first issue demonstrated by this evidence. |
| Miss ledger | Six rows: five SIM_FAIL, one LOW_EV | The failure/decline ledger is small relative to capture volume; inspect its relationship to the funnel before drawing broad conclusions. |

These records establish that the shadow process is alive, receiving data, maintaining current armed slots, and recording evaluations. They do **not** establish a positive expected-value distribution, profitable landed trades, or realized net P&L after gas, L1 data fees, competitive bids, reverts, and lender costs.

### 10.2 New priority P0 — trace the trigger-to-verdict gap

This is now the first engineering investigation. In the armed-verdict file, all 16,021 sampled verdicts have fired=false; all have sim=null, gross_wei=null, an empty per_f, and no populated fire field. Five are nevertheless marked trigger_met=true. Separately, the shadow report describes five funnel closures as failed/Reverted.

Do not assume those two sets of five are the same cases. Establish their relationship by candidate/arm identifier and timestamps, then follow each case through the code path.

**Required work:**

1. Trace all five trigger_met=true verdicts from trigger detection through repricing, quote/size selection, candidate admission, simulation, and final verdict. For each case, identify exactly which stage ran, which stage did not run, and the explicit reason.
2. Join the five failed/Reverted funnel classifications to their source events and determine whether Reverted means a local simulation revert, an internal classifier, a timeout/stale-state failure, or a real on-chain receipt. Do not conflate these.
3. Explain why the verdict schema leaves sim, gross_wei, and per_f empty for trigger-met cases. If they are intentionally not populated in shadow mode, record the reason explicitly and add separate downstream counters that make the omission distinguishable from an unvisited stage or telemetry bug.
4. Add or strengthen a deterministic regression test for each relevant outcome: trigger observed but not actionable; no profitable size; simulation failure; simulation success; stale/invalidation rejection; and dispatch-to-null. Preserve the existing fail-closed checks.
5. Re-run read-only shadow capture and verify every funnel outcome has a consistent terminal reason, with matched counts and no unexplained gaps between trigger, candidate, simulation, verdict, and ledger.

**Acceptance criterion:** Every trigger-met candidate and every funnel closure can be explained end-to-end by stable identifier, stage timestamps, and an unambiguous terminal outcome. “No trade” can be a correct result; an unexplained or unmeasurable “no trade” cannot inform profit optimization.

### 10.3 New priority P1 — instrument latency with matched samples

The seven rows in the current race ledger show receive-to-proposed latency with a median of approximately **104.9 ms**, a p90 of approximately **111.2 ms**, and a maximum of approximately **142.9 ms**. With only seven observations, these are diagnostic signals, not stable performance estimates. The sample is small enough that it should not by itself justify a redesign, but it is sufficient to warrant investigation.

Earlier PLAN measurements reported lower receive-to-proposed medians in a different/smaller sample. Do not compare those results until the build, host, timestamp definitions, and measurement window are matched.

Record p50/p90/p99 and sample count for the same candidate across: event received, trigger detected, route/size proposal, exact quote complete, candidate admitted, simulation complete, and final decision. Separate queue wait from CPU time and RPC/simulation time. Optimize the measured dominant stage only after the trigger-to-verdict gap above is understood.

### 10.4 New priority P1 — make near-miss counters decision-grade

The latest shadow report exposes 80,224 measured near-miss observations, best_bps=6.74, pays=10, plus threshold counters of 24 within 0.5 bps, 65 within 1 bps, 1,898 within 2 bps, 12,051 within 5 bps, 23,952 within 10 bps, and 42,224 beyond 10 bps. These are raw report fields, not proof of 80,224 unique arbitrage opportunities; the code's precise counting and denominator semantics must be confirmed before using them to prioritize strategy changes.

Document the definition of each field, whether buckets are disjoint, what “pays” means, what the basis-point gap is measured against, and whether observations are deduplicated by candidate/block/pool/route. Reconcile their totals against the emitted rows. Then break misses down by explicit reason: price moved, no profitable size, gas/L1 cost, lender capacity, failed quote, failed simulation, state staleness, latency, and estimated competitive priority fee. Rank the causes by **counterfactual net WETH/USD left on the table**, using conservative, replayable estimates rather than raw counts.

### 10.5 What the new evidence changes in the plan

The recommended order is revised as follows:

1. **Explain the trigger-to-verdict and failure-classification gap** (Section 10.2). This is the highest-confidence immediate task from the current telemetry.
2. **Make the profit/miss funnel measurable end-to-end**, including matched stage timings and fully defined near-miss counters. Preserve candidate IDs and terminal reasons; alert on dropped ledger rows.
3. **Quantify latency with a comparable sample**, then optimize only the dominant measured stage.
4. **Return to the economic model questions from the original audit**, including the expected-failure-cost/scenario-prior accounting test and measured bid/landing assumptions. The AWS snapshot does not prove or disprove those code-level concerns.
5. **Continue to obey the existing shadow/live gates.** The present evidence does not meet a bar for claiming profitability or for enabling broadcasting. Do not loosen risk thresholds, increase hop limits, add venues, or increase bidding merely to make the fire count rise.
6. **Harden AWS access for ongoing analysis:** replace broad/root-level analytics access with a dedicated least-privilege role limited to the telemetry and read-only control-plane queries needed. Do not add SSH ingress, export secrets, or put raw calldata/private transaction payloads into a general-purpose analytics store.

The existing gate requirements in the [armed-backruns design](https://github.com/batman3699/arbot-main2/blob/main/docs/superpowers/specs/2026-10-13-armed-backruns-design.md) remain authoritative. This addendum does not authorize live sending or change runtime configuration.

### 10.6 Follow-up measurements

The next report should include, at minimum: counts and unique candidate IDs for every funnel stage; all five trigger-met cases with terminal reasons; reconciliation of the five failed/Reverted closures against their underlying evidence; quote and simulation completion rates; stage latency p50/p90/p99 with sample sizes; near-miss denominator/bucket validation; and conservative counterfactual net EV for genuinely executable opportunities. A new venue or route-search feature should be approved only if this evidence identifies a specific coverage gap and holdout replay shows incremental positive net value.

