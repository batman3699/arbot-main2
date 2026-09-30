//! `Economics`, wired to `apex-econ`'s sizing, cost model and scenario EV.
//!
//! # The join is where a candidate comes into existence
//!
//! §46.2's four stages run concurrently and their join is what turns a
//! `RouteProposal` into a `Candidate`. The plane owns *when* they run;
//! [`LiveEconomics::assemble`] owns *what the numbers mean*, and that is the
//! whole reason the port has an `assemble` rather than letting the plane build a
//! candidate from four values.
//!
//! # The size is minted in exactly one place, and it is not here
//!
//! `apex_econ::sizing::discrete::refine` is the only function in the workspace
//! that can produce a `DiscreteSize`, because it is the only one that can
//! construct the `DiscreteRefined` witness (INV-18,
//! `scripts/ci/no_unearned_discrete_size.sh`). This module calls it and carries
//! the result. The proposal's `size_hint` is an input to the search, never a
//! substitute for it — Engine C found a size against the state *it* saw, and the
//! state has moved by the time this runs.
//!
//! # Scenario priors are pessimistic and flagged as unmeasured
//!
//! §14.1: until Phase 9's competitor model exists, the scenario set is the
//! conservative subset — `SameStateImmediate`, `SameStateOneFlashblockLater`,
//! `CompetingSamePoolSwapFirst`, `VenueRevert` — with "pessimistic fixed priors
//! recorded in config and flagged `prior=unmeasured` in telemetry".
//! `ScenarioSet::prior` is that flag, and [`ScenarioPriors`] is that config. The
//! flag is carried rather than inferred: a measured distribution and a guessed
//! one produce the same `f64` and must never be read as the same claim.
//!
//! # `Pr(Π > 0)` is computed here, because nothing else can compute it
//!
//! `Candidate::probability_of_profit_ppm` exists because `LiveRiskGate` had
//! nothing to read for §2.1's robust gate and was using `capture_probability` —
//! a different quantity. This is where the right one is produced, from the same
//! scenario set that produces the EV, so the two cannot disagree about which
//! world distribution they describe.

use crate::plane::{Decline, Economics, Refinement};
use alloy_primitives::U256 as AlloyU256;
use apex_econ::cost::failure::{expected_failure_cost, FailureProfile};
use apex_econ::cost::l1_data::{CompressedSize, L1DataFee, L1FeeModel, L1FeeParameters};
use apex_econ::ev::scenario::{
    probability_of_profit_ppm, scenario_ev, PriorSource, Profit, Scenario, ScenarioKind,
    ScenarioSet,
};
use apex_econ::sizing::continuous::{optimize, ContinuousBudget};
use apex_econ::sizing::discrete::{refine_detailed, RefineBudget};
use apex_math::finite_size::{SizedRoute, Surplus};
use apex_search::frontier::RouteProposal;
use apex_types::candidate::{Candidate, DiscreteSize};
use apex_types::compat::{u256_to_alloy, u256_to_ethers};
use apex_types::cost::{GasDistribution, GasLimit, GasUsed, TotalExecutionCost};
use apex_types::ids::CandidateId;
use apex_types::route::CertificateStatus;
use apex_types::sim::SimulationTier;
use ethers_core::types::U256 as EthersU256;

/// Prices a proposal's route as a finite-size curve, against live venue state.
///
/// The same seam Engine C uses, and for the same reason: a curve is venue
/// knowledge, and pricing one needs pool state this crate has no business
/// holding. `apex-venues` implements it; a test double stands in for it.
pub trait RouteCurves: Send + Sync {
    /// The route as a curve, or why it cannot be priced right now.
    ///
    /// Returning the *evaluation* rather than a boxed `SizedRoute` keeps the
    /// lifetime out of the trait: a `Box<dyn SizedRoute + '_>` borrowed from live
    /// state cannot cross an `async` boundary without the caller owning the
    /// state, and the caller is the plane.
    fn evaluate(
        &self,
        p: &RouteProposal,
        f: &mut dyn FnMut(&dyn SizedRoute) -> Result<Evaluated, Decline>,
    ) -> Result<Evaluated, Decline>;
}

/// What one pass over a route's curve produced.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Evaluated {
    pub size: DiscreteSize,
    pub amount: EthersU256,
    pub output: EthersU256,
    pub net: Surplus,
    pub evaluations: u32,
}

/// §14.1's pessimistic fixed priors, and the gross-to-scenario mapping.
///
/// Every probability is integer ppm. §14.1 says the priors live in config, and
/// `apex-config` is where they will come from; the defaults here are the values
/// this repository can defend from measurement, and each carries its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScenarioPriors {
    /// The trade executes against the state it was priced at.
    pub same_state_ppm: u32,
    /// One Flashblock later — Base emits one every ~200 ms.
    pub one_flashblock_later_ppm: u32,
    /// A competing swap touches the same pool first.
    pub competing_swap_first_ppm: u32,
    /// The venue reverts.
    pub venue_revert_ppm: u32,
    /// What fraction of the gross survives a one-Flashblock delay, in bps.
    pub delayed_gross_bps: u32,
    /// What fraction survives a competitor touching the pool first, in bps.
    pub competed_gross_bps: u32,
}

impl Default for ScenarioPriors {
    /// **Pessimistic, and each number has a source.**
    ///
    /// The event census found that 88% of net-positive samples followed a swap
    /// and that the control (quiet blocks) had a median of −1.64 bps against the
    /// swap-triggered +0.04 — so most of the probability mass belongs on worlds
    /// where something else moved first, not on the world where nothing did.
    /// `same_state_ppm` is therefore a minority of the distribution rather than
    /// the default assumption a naive model makes.
    ///
    /// These are priors, not measurements, and `PriorSource::Unmeasured` says so
    /// on every set they produce. Phase 9 replaces them.
    fn default() -> Self {
        Self {
            same_state_ppm: 250_000,
            one_flashblock_later_ppm: 350_000,
            competing_swap_first_ppm: 350_000,
            venue_revert_ppm: 50_000,
            // A Flashblock of decay. Measured: edges persisted ~35 s in this
            // repository's own sampling, so one 200 ms block costs little — but
            // "little" is not "nothing", and a prior that said nothing would make
            // the delayed world indistinguishable from the immediate one.
            delayed_gross_bps: 9_500,
            // A competitor touching the pool first takes most of it. The census
            // measured the cheap frontier as efficient to within ~0.18 bps, which
            // is what "somebody else got there" costs.
            competed_gross_bps: 1_000,
        }
    }
}

impl ScenarioPriors {
    /// The §14.1 subset, with this route's gross mapped into each world.
    ///
    /// `gross_wei` is what the route makes if nothing interferes;
    /// `irrecoverable_wei` is what is spent regardless — the L1 data fee of a
    /// transaction that was included and reverted is the canonical case, and it
    /// sits outside the sum rather than inside each scenario's profit.
    pub fn scenario_set(&self, gross_wei: i128, cost_wei: i128) -> ScenarioSet {
        let scaled = |bps: u32| -> i128 {
            gross_wei.saturating_mul(i128::from(bps)) / 10_000
        };
        ScenarioSet {
            scenarios: vec![
                Scenario {
                    kind: ScenarioKind::SameStateImmediate,
                    probability_ppm: self.same_state_ppm,
                    profit: Profit(gross_wei.saturating_sub(cost_wei)),
                },
                Scenario {
                    kind: ScenarioKind::SameStateOneFlashblockLater,
                    probability_ppm: self.one_flashblock_later_ppm,
                    profit: Profit(scaled(self.delayed_gross_bps).saturating_sub(cost_wei)),
                },
                Scenario {
                    kind: ScenarioKind::CompetingSamePoolSwapFirst,
                    probability_ppm: self.competing_swap_first_ppm,
                    profit: Profit(scaled(self.competed_gross_bps).saturating_sub(cost_wei)),
                },
                Scenario {
                    kind: ScenarioKind::VenueRevert,
                    probability_ppm: self.venue_revert_ppm,
                    // A revert makes no gross and still pays the cost. Not zero:
                    // a scenario whose profit was zero would make a revert look
                    // free, and the failure branch is exactly what §23.1's
                    // `expected_failure_cost` exists to price.
                    profit: Profit(-cost_wei),
                },
            ],
            prior: PriorSource::Unmeasured,
        }
    }
}

/// The chain-side numbers the cost model needs, as of a block.
///
/// Carried rather than fetched: this crate does no I/O, and the read belongs to
/// `apex-chain`. Same argument `PoolAdmission`'s `BytecodeEvidence` makes.
///
/// No `PartialEq`: `L1FeeModel` has none, and that is its own choice — two
/// models are not usefully equal, because what matters about one is the evidence
/// behind it and comparing those by value invites treating a validated model and
/// an unvalidated one as interchangeable when their fields happen to line up.
#[derive(Clone, Copy, Debug)]
pub struct ChainCosts {
    pub gas_price_wei: AlloyU256,
    pub l1: L1FeeParameters,
    /// **Provenance is an input, not a choice this module makes.** `L1FeeModel`
    /// carries whether the Fjord implementation has been checked against
    /// receipts, and `L1DataFee::is_authoritative` additionally requires a
    /// *measured* size — a validated model fed an estimated size is still an
    /// estimate. The candidate this module assembles is `Heuristic` partly
    /// because of that, and `an_estimated_l1_fee_is_not_authoritative` is the
    /// test.
    pub l1_model: L1FeeModel,
    pub failure: FailureProfile,
    /// The gas a successful settlement of this shape uses. §12.1's `GasClass` is
    /// a bucket for ranking; this is the number the cost model prices against.
    pub success_gas: GasUsed,
    pub gas_limit: GasLimit,
}

/// §46.2's four stages, over `apex-econ`.
pub struct LiveEconomics {
    curves: std::sync::Arc<dyn RouteCurves>,
    priors: ScenarioPriors,
    costs: ChainCosts,
    continuous: ContinuousBudget,
    refine: RefineBudget,
    strategy: apex_types::ids::StrategyId,
}

impl LiveEconomics {
    pub fn new(
        curves: std::sync::Arc<dyn RouteCurves>,
        priors: ScenarioPriors,
        costs: ChainCosts,
        strategy: apex_types::ids::StrategyId,
    ) -> Self {
        Self {
            curves,
            priors,
            costs,
            continuous: ContinuousBudget::default(),
            refine: RefineBudget::default(),
            strategy,
        }
    }

    pub const fn priors(&self) -> ScenarioPriors {
        self.priors
    }

    /// One pass: continuous warm start, then discrete refinement.
    ///
    /// The four port methods each call this. That is four evaluations of the same
    /// curve where one would do, and it is a deliberate cost: §46.2 requires the
    /// four stages to be *independently* runnable so the plane can join them
    /// concurrently, and a shared cache between them would be shared mutable
    /// state on the capture path (INV-11). Whether the duplication is worth the
    /// concurrency is a measurement Phase 8's benchmark table will settle; until
    /// then the honest version is the one that matches the contract.
    fn evaluate(&self, p: &RouteProposal) -> Result<Evaluated, Decline> {
        self.curves.evaluate(p, &mut |route| {
            let Some(start) = optimize(route, self.continuous) else {
                return Err(Decline::SimulationFailed { class: None });
            };
            let Some(found) = refine_detailed(route, start, self.refine) else {
                return Err(Decline::SimulationFailed { class: None });
            };
            match found.size {
                Some(size) => Ok(Evaluated {
                    size,
                    amount: found.amount,
                    output: found.output,
                    net: found.net,
                    evaluations: found.evaluations + start.evaluations(),
                }),
                // Priced at every size and profitable at none. **The 96% bucket**,
                // and `LowEv` is what it is — the route priced fine and does not
                // pay, which is a different fact from a venue that would not
                // price it.
                None => Err(Decline::NoProfitableSize),
            }
        })
    }

    /// §23's total, for a route of this shape.
    fn total_cost(&self, calldata_bytes: u32) -> TotalExecutionCost {
        let fee = self.l1_fee(calldata_bytes);
        let l1_wei = u256_to_alloy(fee.wei);
        let failure = expected_failure_cost(
            self.costs.failure,
            u256_to_ethers(self.costs.gas_price_wei),
            fee.wei,
        );
        let l2 = AlloyU256::from(self.costs.success_gas.0)
            .saturating_mul(self.costs.gas_price_wei);

        TotalExecutionCost {
            l2_execution_fee: to_u128(l2),
            l1_data_fee: to_u128(l1_wei),
            // §4.5: a priority fee on Base ranks within the sequencer's window.
            // Zero here because nothing measured what it buys, and a fabricated
            // bid is a cost the EV would then have to clear.
            priority_fee: 0,
            builder_payment: 0,
            sequencer_payment: 0,
            flash_fee: 0,
            dex_fees: 0,
            expected_failure_cost: to_u128(u256_to_alloy(failure)),
            calldata_bytes,
            compressed_data_estimate: calldata_bytes,
            gas_limit: self.costs.gas_limit,
            // §23.1 requires a distribution: the risk gate prices at p99 while
            // the EV uses p50, and a single number cannot serve both. The spread
            // is a shape rather than a measurement, which is why the venue's
            // `GasProfile::measured` is false and why the candidate is
            // `Heuristic`.
            gas_used_distribution: GasDistribution {
                p50: self.costs.success_gas,
                p90: GasUsed(self.costs.success_gas.0.saturating_mul(11) / 10),
                p99: GasUsed(self.costs.success_gas.0.saturating_mul(13) / 10),
                max_observed: GasUsed(self.costs.gas_limit.0),
            },
        }
    }

    /// Everything a route of `hops` hops costs, in wei: the figure `assemble`
    /// subtracts from the gross.
    ///
    /// Exposed so a pricer's `SizedRoute::fixed_cost` is **the same number**. A
    /// search that sized routes against one cost while the economics charged
    /// another would propose routes the economics then refuses, and miss ones it
    /// would have taken.
    pub fn route_cost_wei(&self, hops: usize) -> u128 {
        u128::try_from(cost_i128(&self.total_cost(calldata_for_hops(hops)))).unwrap_or(u128::MAX)
    }

    /// The Fjord fee, with its provenance attached.
    ///
    /// `CompressedSize::Estimated` rather than `Measured`: the real fastlz size
    /// comes from the encoded transaction, which is `apex-exec`'s. An estimated
    /// size makes the fee non-authoritative even against a validated model, and
    /// that is the honest state for a route nothing has encoded yet.
    pub fn l1_fee(&self, calldata_bytes: u32) -> L1DataFee {
        self.costs
            .l1_model
            .fee(CompressedSize::Estimated(calldata_bytes), self.costs.l1)
    }
}

fn to_u128(v: AlloyU256) -> u128 {
    u128::try_from(v).unwrap_or(u128::MAX)
}

#[async_trait::async_trait]
impl Economics for LiveEconomics {
    async fn reprice(&self, p: &RouteProposal) -> Result<AlloyU256, Decline> {
        Ok(u256_to_alloy(self.evaluate(p)?.output))
    }

    async fn size(&self, p: &RouteProposal) -> Result<DiscreteSize, Decline> {
        Ok(self.evaluate(p)?.size)
    }

    /// §2.1's robustness margin: how far `J` sits above zero as a fraction of
    /// the gross.
    ///
    /// Computed from the scenario set rather than from the immediate world, which
    /// is the point of having one: a route whose profit survives only the
    /// no-interference scenario has a thin margin however large that scenario's
    /// profit is.
    async fn scenarios(&self, p: &RouteProposal) -> Result<f64, Decline> {
        let e = self.evaluate(p)?;
        let gross = surplus_to_i128(e.net).saturating_add(cost_i128(&self.total_cost(
            calldata_estimate(p),
        )));
        let set = self.priors.scenario_set(gross, cost_i128(&self.total_cost(calldata_estimate(p))));
        let ev = scenario_ev(&set, EthersU256::zero())
            .map_err(|e| Decline::Uncommittable { detail: format!("scenario set: {e:?}") })?;
        if gross <= 0 {
            return Ok(0.0);
        }
        #[allow(clippy::cast_precision_loss)]
        Ok((ev as f64 / gross as f64).clamp(0.0, 1.0))
    }

    async fn refresh_costs(&self, p: &RouteProposal) -> Result<TotalExecutionCost, Decline> {
        Ok(self.total_cost(calldata_estimate(p)))
    }

    fn assemble(&self, p: &RouteProposal, r: Refinement) -> Result<Candidate, Decline> {
        let cost = cost_i128(&r.costs);
        // **The gross is what the cycle returns above what it took.**
        // `expected_output` is `reprice`'s answer — the curve's OUTPUT at the
        // refined size, input included. The first version used it as the gross
        // itself, so a 1-WETH cycle returning 1.002 WETH read as a 1.002-WETH
        // profit and every candidate looked profitable by roughly its own size.
        // Every unit test built a `Refinement` by hand with a small
        // `expected_output`, so the join the plane actually runs was never
        // exercised; `the_gross_is_what_the_cycle_returns_above_what_it_took`
        // runs it.
        //
        // Same token on both sides, and the costs are wei, so this is exact only
        // for a cycle denominated in the native token — which is why the live
        // frontier prices WETH-start cycles until this module takes a price.
        let gross = i128::try_from(to_u128(r.expected_output))
            .unwrap_or(i128::MAX)
            .saturating_sub(i128::try_from(to_u128(r.input_amount.get())).unwrap_or(i128::MAX));
        let set = self.priors.scenario_set(gross, cost);
        let robust_ev = scenario_ev(&set, EthersU256::zero())
            .map_err(|e| Decline::Uncommittable { detail: format!("scenario set: {e:?}") })?;
        let pr_profit = probability_of_profit_ppm(&set)
            .map_err(|e| Decline::Uncommittable { detail: format!("scenario set: {e:?}") })?;

        // **§20's Tier 0 screen, and it belongs here.**
        //
        // The first draft put it in the plane, which had to *recover* the gas
        // price by dividing `l2_execution_fee` by p50 gas — and integer division
        // of a fee by a gas count gives zero whenever the fee is smaller, which
        // silently turned the conservative total into its non-gas components. The
        // screen needs a price, and this is the module that holds one.
        //
        // `Tier0Verdict::Escalate` carries its own warning: *"Not a prediction
        // that it will succeed."* §20: *"A lower tier may only reject. It may
        // never admit something a higher tier would have caught, because nothing
        // runs after it to catch anything."* So a pass here means only that a
        // higher tier is worth its cost — and it is why this cannot satisfy the
        // `Simulator` port, which would require inventing `success`,
        // `loan_repaid`, `profit_invariant_held` and `state_after`, three of which
        // `LiveRiskGate` reads.
        //
        // Refusing during `assemble` means a screened-out candidate **never
        // becomes a ticket**, so INV-01 has nothing to account for.
        if let apex_sim::tier0::Tier0Verdict::Reject { .. } =
            apex_sim::tier0::screen(&apex_sim::tier0::Tier0Input {
                gross_profit_wei: gross,
                cost: r.costs.clone(),
                gas_price_wei: to_u128(self.costs.gas_price_wei),
            })
        {
            return Err(Decline::NoProfitableSize);
        }

        Ok(Candidate {
            // Derived from the route hash, so the same route against the same
            // state gets the same id and the miss ledger can join records across
            // a retry. Not a counter: two processes with counters produce
            // colliding ids for different candidates.
            candidate_id: CandidateId(u64::from_be_bytes(
                p.route.route_hash.0[..8].try_into().unwrap_or([0u8; 8]),
            )),
            chain_id: p.chain,
            strategy: self.strategy,
            venue_set: p.venue_set.clone(),
            route: p.route.clone(),
            state_fingerprint: p.state_fingerprint.clone(),
            state_age: apex_types::time::DurationNanos(0),
            flash_source: None,
            input_amount: r.input_amount,
            expected_output: r.expected_output,
            gross_profit: AlloyU256::from(u128::try_from(gross.max(0)).unwrap_or(u128::MAX)),
            dex_fees: AlloyU256::ZERO,
            flash_fee: AlloyU256::ZERO,
            total_execution_cost: r.costs,
            expected_net_profit: gross.saturating_sub(cost),
            robust_ev,
            // **`Heuristic`, not `Proven`.** §16.2: never silently promote a
            // heuristic allocation to optimal. A single-route size found by
            // golden section plus a discrete climb is not a certified optimum —
            // §16's `certify` is what produces `Proven`, and it arrives in
            // Phase 12. INV-17 then keeps this out of live dispatch, which is
            // correct: a shadow run should measure what this finds, not trade it.
            certificate_status: CertificateStatus::Heuristic,
            // The tier that has actually run. `Simulator` sets the real one.
            simulation_tier: SimulationTier::Tier0Analytic,
            // P(lands). Not this module's to estimate — §21.2's capture curve is
            // Phase 9 — so it is zero and says nothing rather than guessing.
            capture_probability: 0.0,
            robustness_margin: r.robustness_margin,
            probability_of_profit_ppm: pr_profit,
            deadline: apex_types::time::UnixNanos(p.found_at.0.saturating_add(2_000_000_000)),
            submission_policy: apex_types::ticket::SubmissionPolicy::Private,
        })
    }
}

/// Everything spent, in wei.
fn cost_i128(c: &TotalExecutionCost) -> i128 {
    let total = c
        .l2_execution_fee
        .saturating_add(c.l1_data_fee)
        .saturating_add(c.priority_fee)
        .saturating_add(c.builder_payment)
        .saturating_add(c.sequencer_payment)
        .saturating_add(c.flash_fee)
        .saturating_add(c.dex_fees)
        .saturating_add(c.expected_failure_cost);
    i128::try_from(total).unwrap_or(i128::MAX)
}

fn surplus_to_i128(s: Surplus) -> i128 {
    match s {
        Surplus::Gain(v) => i128::try_from(to_u128(u256_to_alloy(v))).unwrap_or(i128::MAX),
        Surplus::Loss(v) => -i128::try_from(to_u128(u256_to_alloy(v))).unwrap_or(i128::MAX),
    }
}

/// Calldata size for a route of this shape.
///
/// A measured constant per hop rather than an encoding: the real number comes
/// from `apex-exec`'s encoder, and this is the estimate §23.3's optimizer
/// compares candidates with. `CompressedSize::Estimated` is what carries that
/// distinction into the fee.
fn calldata_estimate(p: &RouteProposal) -> u32 {
    calldata_for_hops(p.route.hops.len())
}

fn calldata_for_hops(hops: usize) -> u32 {
    const PER_HOP_BYTES: u32 = 196;
    const OVERHEAD_BYTES: u32 = 132;
    OVERHEAD_BYTES.saturating_add(
        PER_HOP_BYTES.saturating_mul(u32::try_from(hops).unwrap_or(u32::MAX)),
    )
}
