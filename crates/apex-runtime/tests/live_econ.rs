//! `Economics` over `apex-econ`: the sizing, the cost model, and the scenario set.
//!
//! The substance is in three places, and each is somewhere a plausible shortcut
//! would have produced a number nobody measured:
//!
//! 1. **The size is minted by `apex-econ` and nowhere else** (INV-18). The
//!    proposal's `size_hint` is an input to the search, not a substitute for it.
//! 2. **The scenario priors are pessimistic and flagged unmeasured** (§14.1).
//! 3. **`Pr(Π > 0)` comes from the same set as the EV**, so the two cannot
//!    disagree about which world distribution they describe.

mod support;

use alloy_primitives::{Address, B256, U256 as AlloyU256};
use apex_econ::cost::failure::FailureProfile;
use apex_econ::cost::l1_data::{L1FeeModel, L1FeeParameters};
use apex_econ::ev::scenario::{PriorSource, ScenarioKind};
use apex_math::finite_size::SizedRoute;
use apex_runtime::econ::{ChainCosts, Evaluated, LiveEconomics, RouteCurves, ScenarioPriors};
use apex_runtime::plane::{Decline, Economics, Refinement};
use apex_search::frontier::{ProposalOrigin, RouteProposal};
use apex_types::cost::{GasLimit, GasUsed};
use apex_types::ids::StrategyId;
use apex_types::route::{CertificateStatus, ComplexityCost, RouteCommitment, RouteHop};
use apex_types::sim::SimulationTier;
use apex_types::time::UnixNanos;
use ethers_core::types::U256 as EthersU256;
use std::sync::Arc;
use support::*;

/// A real constant-product round trip, as `apex-math`'s evaluator sees it.
#[derive(Clone, Copy)]
struct RoundTrip {
    reserve: u128,
    spread_bps: u64,
    fee_bps: u64,
    fixed_cost: u64,
}

impl RoundTrip {
    fn cp(&self, amount_in: EthersU256, r_in: u128, r_out: u128) -> Option<EthersU256> {
        let with_fee =
            amount_in.checked_mul(EthersU256::from(10_000 - self.fee_bps))? / EthersU256::from(10_000u64);
        let num = with_fee.checked_mul(EthersU256::from(r_out))?;
        let den = EthersU256::from(r_in).checked_add(with_fee)?;
        (!den.is_zero()).then(|| num / den)
    }
}

impl SizedRoute for RoundTrip {
    fn output(&self, amount_in: EthersU256) -> Option<EthersU256> {
        let mid = self.cp(amount_in, self.reserve, self.reserve)?;
        let tilted = self.reserve.checked_mul(10_000 + u128::from(self.spread_bps))? / 10_000;
        self.cp(mid, self.reserve, tilted)
    }
    fn fixed_cost(&self) -> EthersU256 {
        EthersU256::from(self.fixed_cost)
    }
    fn max_input(&self) -> EthersU256 {
        EthersU256::from(self.reserve / 10)
    }
}

struct Curves {
    route: Option<RoundTrip>,
}

impl RouteCurves for Curves {
    fn evaluate(
        &self,
        _p: &RouteProposal,
        f: &mut dyn FnMut(&dyn SizedRoute) -> Result<Evaluated, Decline>,
    ) -> Result<Evaluated, Decline> {
        match &self.route {
            Some(r) => f(r),
            None => Err(Decline::SimulationFailed { class: None }),
        }
    }
}

const GAS: u64 = 20_000_000_000_000;

fn chain_costs(model: L1FeeModel) -> ChainCosts {
    ChainCosts {
        gas_price_wei: AlloyU256::from(6_000_000u64),
        // Real Base values, from the tracked receipt in
        // crates/apex-chain/tests/fixtures/base_receipt.json.
        l1: L1FeeParameters {
            l1_base_fee: EthersU256::from(0x0814_1375u64),
            l1_blob_base_fee: EthersU256::from(0x7e_d644u64),
            base_fee_scalar: 0x8dd,
            blob_base_fee_scalar: 0x0010_1c12,
        },
        l1_model: model,
        failure: FailureProfile { gas_on_failure: GasUsed(90_000), failure_ppm: 50_000 },
        success_gas: GasUsed(240_000),
        gas_limit: GasLimit(400_000),
    }
}

fn econ_over(spread_bps: u64, model: L1FeeModel) -> LiveEconomics {
    LiveEconomics::new(
        Arc::new(Curves {
            route: Some(RoundTrip {
                reserve: 1_000_000_000_000_000_000_000,
                spread_bps,
                fee_bps: 30,
                fixed_cost: GAS,
            }),
        }),
        ScenarioPriors::default(),
        chain_costs(model),
        StrategyId(1),
    )
}

fn proposal(hops: usize) -> RouteProposal {
    let token = |n: u8| apex_types::ids::TokenId { chain: BASE, address: Address::repeat_byte(n) };
    RouteProposal {
        chain: BASE,
        route: RouteCommitment {
            hops: (0..hops)
                .map(|i| RouteHop {
                    venue: VENUE,
                    pool: apex_types::ids::PoolId {
                        chain: BASE,
                        #[allow(clippy::cast_possible_truncation)]
                        address: Address::repeat_byte(0x33 + i as u8),
                    },
                    token_in: token(0x01),
                    token_out: token(0x02),
                    fee_ppm: 500,
                })
                .collect(),
            complexity_cost: ComplexityCost {
                hops: 2,
                external_calls: 2,
                calldata_bytes: 420,
                state_deps: 2,
                tick_crossings: 0,
                hooks: 0,
                gas_estimate: 240_000,
                failure_surface: 0.01,
            },
            route_hash: B256::repeat_byte(0x44),
        },
        venue_set: vec![VENUE],
        state_fingerprint: fingerprint(47_079_437, 1),
        found_at: UnixNanos(1_000_000_000),
        origin: ProposalOrigin::FiniteSize,
        flash_source: None,
        size_hint: Some(AlloyU256::from(1u64)),
    }
}

/// **The size comes from the refinement, not from the hint.**
///
/// Engine C found a size against the state *it* saw; the state has moved by the
/// time this runs. The hint being wildly wrong must not change the answer.
#[tokio::test]
async fn the_size_is_refined_rather_than_taken_from_the_hint() {
    let econ = econ_over(200, L1FeeModel::unvalidated());

    let mut p = proposal(2);
    p.size_hint = Some(AlloyU256::from(1u64));
    let from_tiny = econ.size(&p).await.expect("sizable");

    p.size_hint = Some(AlloyU256::from(u128::MAX));
    let from_huge = econ.size(&p).await.expect("sizable");

    assert_eq!(
        from_tiny, from_huge,
        "the hint is an input to the search, not the answer"
    );
    assert!(from_tiny.get() > AlloyU256::from(1u64), "and it is not the hint");
}

/// **The 96% bucket.** A spread that cannot clear the fixed cost declines with
/// `NoProfitableSize`, distinct from a venue that would not price it.
#[tokio::test]
async fn a_spread_that_cannot_clear_the_cost_has_no_profitable_size() {
    let econ = econ_over(61, L1FeeModel::unvalidated());
    let err = econ.size(&proposal(2)).await.expect_err("61 bps cannot clear ~1 cent of gas");
    assert_eq!(err, Decline::NoProfitableSize);

    // Distinct from an unpriceable route: one is economics, the other is a
    // broken adapter, and folding them hides the second inside the first.
    let unpriceable = LiveEconomics::new(
        Arc::new(Curves { route: None }),
        ScenarioPriors::default(),
        chain_costs(L1FeeModel::unvalidated()),
        StrategyId(1),
    );
    assert_eq!(
        unpriceable.size(&proposal(2)).await.expect_err("no curve"),
        Decline::SimulationFailed { class: None }
    );
}

/// **§14.1's priors are the conservative subset and are flagged unmeasured.**
///
/// A measured distribution and a guessed one produce the same `f64` and must
/// never be read as the same claim.
#[test]
fn the_scenario_set_is_the_phase_3_subset_flagged_unmeasured() {
    let set = ScenarioPriors::default().scenario_set(1_000_000, 100_000);

    assert_eq!(set.prior, PriorSource::Unmeasured, "these are priors, not measurements");
    assert!(!set.is_measured());
    set.validate().expect("the probabilities must partition the outcome space");

    let kinds: Vec<ScenarioKind> = set.scenarios.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        ScenarioKind::PHASE_3_SUBSET.to_vec(),
        "exactly §14.1's four, in its order"
    );
}

/// **Most of the probability mass is not on the no-interference world**, and that
/// is what the event census measured: 88% of net-positive samples followed a
/// swap, and the quiet-block control had a median of −1.64 bps.
#[test]
fn the_priors_do_not_assume_nothing_interferes() {
    let priors = ScenarioPriors::default();
    let set = priors.scenario_set(1_000_000, 0);

    let same_state = set
        .scenarios
        .iter()
        .find(|s| s.kind == ScenarioKind::SameStateImmediate)
        .expect("present");
    assert!(
        same_state.probability_ppm < 500_000,
        "a naive model puts the mass here; the measurements do not"
    );

    // And a competitor getting there first takes most of the gross.
    let competed = set
        .scenarios
        .iter()
        .find(|s| s.kind == ScenarioKind::CompetingSamePoolSwapFirst)
        .expect("present");
    assert!(competed.profit.0 < same_state.profit.0 / 2, "{competed:?} vs {same_state:?}");
}

/// **A revert makes no gross and still pays the cost.** A scenario whose profit
/// was zero would make a revert look free.
#[test]
fn a_revert_scenario_still_pays_the_cost() {
    let set = ScenarioPriors::default().scenario_set(1_000_000, 100_000);
    let revert = set
        .scenarios
        .iter()
        .find(|s| s.kind == ScenarioKind::VenueRevert)
        .expect("present");
    assert_eq!(revert.profit.0, -100_000, "the cost is paid, the gross is not made");
}

/// **`Pr(Π > 0)` and the EV come from the same set**, so they cannot disagree
/// about which world distribution they describe.
#[tokio::test]
async fn the_profit_probability_and_the_ev_share_one_distribution() {
    let econ = econ_over(200, L1FeeModel::unvalidated());
    let p = proposal(2);
    let size = econ.size(&p).await.expect("sizable");
    let refinement = Refinement {
        expected_output: size.get() + AlloyU256::from(2_000_000_000_000_000u64),
        input_amount: size,
        robustness_margin: econ.scenarios(&p).await.expect("scenarios"),
        costs: econ.refresh_costs(&p).await.expect("costs"),
    };
    let c = econ.assemble(&p, refinement).expect("assembles");

    // Both derived, neither invented.
    assert!(c.probability_of_profit_ppm <= 1_000_000);
    assert_ne!(
        c.probability_of_profit_ppm, 0,
        "a profitable route has some world in which it profits"
    );
    // And the unmeasured prior is reflected in the candidate being heuristic.
    assert_eq!(c.certificate_status, CertificateStatus::Heuristic);
}

/// **`Heuristic`, never `Proven`.** §16.2: never silently promote a heuristic
/// allocation to optimal. Golden section plus a discrete climb is not a certified
/// optimum — §16's `certify` produces `Proven` and arrives in Phase 12 — and
/// INV-17 then keeps this out of live dispatch, which is correct for a shadow run.
#[tokio::test]
async fn an_assembled_candidate_is_never_proven() {
    let econ = econ_over(200, L1FeeModel::validated(12, 50));
    let p = proposal(2);
    let size = econ.size(&p).await.expect("sizable");
    let refinement = Refinement {
        expected_output: size.get() + AlloyU256::from(2_000_000_000_000_000u64),
        input_amount: size,
        robustness_margin: 0.3,
        costs: econ.refresh_costs(&p).await.expect("costs"),
    };
    let c = econ.assemble(&p, refinement).expect("assembles");

    assert_eq!(
        c.certificate_status,
        CertificateStatus::Heuristic,
        "even with a receipt-validated L1 model, the ALLOCATION is not certified"
    );
    assert_eq!(c.simulation_tier, SimulationTier::Tier0Analytic, "nothing has simulated it yet");
    assert!(
        (c.capture_probability - 0.0).abs() < f64::EPSILON,
        "P(lands) is §21.2's and Phase 9's; zero says nothing rather than guessing"
    );
}

/// **An estimated size makes the L1 fee non-authoritative even against a
/// validated model.** A validated model fed an estimated size is still an
/// estimate, and `is_authoritative` requires both.
#[test]
fn an_estimated_l1_fee_is_not_authoritative() {
    for model in [L1FeeModel::unvalidated(), L1FeeModel::validated(12, 50)] {
        let econ = LiveEconomics::new(
            Arc::new(Curves { route: None }),
            ScenarioPriors::default(),
            chain_costs(model),
            StrategyId(1),
        );
        let fee = econ.l1_fee(460);
        assert!(!fee.wei.is_zero(), "the Fjord fee on real Base parameters is not zero");
        assert!(
            !fee.is_authoritative(),
            "the size is estimated, so the fee may not price a live dispatch"
        );
    }
}

/// The cost grows with hop count, because calldata does. A cost model
/// indifferent to route length would make a 4-hop route look as cheap as a
/// 2-hop one — and the census found deeper routes strictly worse.
#[tokio::test]
async fn a_longer_route_costs_more() {
    let econ = econ_over(200, L1FeeModel::unvalidated());
    let two = econ.refresh_costs(&proposal(2)).await.expect("costs");
    let four = econ.refresh_costs(&proposal(4)).await.expect("costs");

    assert!(four.calldata_bytes > two.calldata_bytes);
    assert!(four.l1_data_fee > two.l1_data_fee, "{} vs {}", four.l1_data_fee, two.l1_data_fee);
}

/// §23.1's distribution: p50 < p90 < p99, because the risk gate prices at p99
/// and the EV uses p50 and one number cannot serve both.
#[tokio::test]
async fn the_gas_distribution_is_a_distribution() {
    let costs = econ_over(200, L1FeeModel::unvalidated())
        .refresh_costs(&proposal(2))
        .await
        .expect("costs");
    let d = costs.gas_used_distribution;
    assert!(d.p50.0 < d.p90.0, "{d:?}");
    assert!(d.p90.0 < d.p99.0, "{d:?}");
    assert!(d.p99.0 <= d.max_observed.0, "{d:?}");
}

/// **The L1 fee is INSIDE the parentheses.**
///
/// `apex-econ`'s own comment: `p_fail * (gas_on_failure * gas_price +
/// l1_data_fee)` — *"with the L1 fee inside the parentheses, not outside, because
/// it is paid whether or not the execution succeeds."*
///
/// The first version of this test asserted `expected_failure_cost > 0`, which gas
/// alone satisfies — a mutation removing the L1 term entirely passed. The
/// assertion has to be quantitative: the figure must exceed what gas alone would
/// give, by the L1 share.
#[tokio::test]
async fn the_failure_branch_prices_the_l1_fee_it_pays_anyway() {
    let econ = econ_over(200, L1FeeModel::unvalidated());
    let costs = econ.refresh_costs(&proposal(2)).await.expect("costs");

    assert!(costs.l1_data_fee > 0, "the Fjord fee on real Base parameters is not zero");

    // What gas alone would contribute: p_fail * gas_on_failure * gas_price.
    let gas_only = 90_000u128 * 6_000_000u128 * 50_000 / 1_000_000;
    assert!(
        costs.expected_failure_cost > gas_only,
        "the L1 fee is paid on a revert too: {} must exceed the gas-only {}",
        costs.expected_failure_cost,
        gas_only
    );

    // And the excess is the L1 share, within the integer division.
    let l1_share = costs.l1_data_fee * 50_000 / 1_000_000;
    let excess = costs.expected_failure_cost - gas_only;
    assert!(
        excess.abs_diff(l1_share) <= 1,
        "the excess {excess} should be the L1 share {l1_share}"
    );
}

/// **§20's Tier 0 screen, in the module that holds a gas price.**
///
/// The first draft put this in the plane, which had to *recover* the price by
/// dividing `l2_execution_fee` by p50 gas — and integer division of a fee by a gas
/// count gives **zero** whenever the fee is smaller, silently reducing the
/// conservative total to its non-gas components. A test caught it by not firing.
///
/// A pass here is not a claim: `Tier0Verdict::Escalate` means only that a higher
/// tier is worth its cost.
#[tokio::test]
async fn a_candidate_that_cannot_pay_for_itself_is_screened_out() {
    let econ = econ_over(200, L1FeeModel::unvalidated());
    let p = proposal(2);
    let costs = econ.refresh_costs(&p).await.expect("costs");

    let size = econ.size(&p).await.expect("sizable");
    // A gross of 1 wei against a real Base cost.
    let thin = Refinement {
        expected_output: size.get() + AlloyU256::from(1u64),
        input_amount: size,
        robustness_margin: 0.3,
        costs: costs.clone(),
    };
    assert_eq!(
        econ.assemble(&p, thin).expect_err("1 wei cannot pay for a Base transaction"),
        Decline::NoProfitableSize
    );

    // The complement, so the pair discriminates: a gross well above the
    // conservative total assembles. Without it, "the screen refuses" is satisfied
    // by a screen that refuses everything.
    let fat = Refinement {
        expected_output: size.get() + AlloyU256::from(500_000_000_000_000u64),
        input_amount: size,
        robustness_margin: 0.3,
        costs,
    };
    assert!(econ.assemble(&p, fat).is_ok());
}

/// **The screen prices at p99, not p50.** §23.1 holds a distribution because the
/// risk gate prices at p99 while the EV uses p50, and `conservative_total` is the
/// p99 reading. A screen using p50 would pass candidates the gate then refuses,
/// spending a simulation on each.
#[tokio::test]
async fn the_screen_prices_at_the_conservative_total() {
    let econ = econ_over(200, L1FeeModel::unvalidated());
    let p = proposal(2);
    let costs = econ.refresh_costs(&p).await.expect("costs");

    let gas_price = 6_000_000u128;
    let at_p50 = u128::from(costs.gas_used_distribution.p50.0) * gas_price;
    let at_p99 = u128::from(costs.gas_used_distribution.p99.0) * gas_price;
    assert!(at_p99 > at_p50, "the distribution must have a spread to test");

    let non_gas = costs.l1_data_fee
        + costs.priority_fee
        + costs.builder_payment
        + costs.sequencer_payment
        + costs.flash_fee
        + costs.dex_fees
        + costs.expected_failure_cost;

    let size = econ.size(&p).await.expect("sizable");
    // A gross that covers p50 but not p99 must be screened out.
    let between = Refinement {
        expected_output: size.get() + AlloyU256::from(at_p50 + non_gas + 1),
        input_amount: size,
        robustness_margin: 0.3,
        costs: costs.clone(),
    };
    assert_eq!(
        econ.assemble(&p, between).expect_err("covers p50, not p99"),
        Decline::NoProfitableSize,
        "a screen using p50 would have passed this and spent a simulation on it"
    );

    // And one that covers p99 passes.
    let above = Refinement {
        expected_output: size.get() + AlloyU256::from(at_p99 + non_gas + 1),
        input_amount: size,
        robustness_margin: 0.3,
        costs,
    };
    assert!(econ.assemble(&p, above).is_ok());
}

/// **The seam between `reprice` and `assemble`.** `Refinement::expected_output`
/// is `reprice`'s answer: the curve's *output* at the refined size, input
/// included. The gross is what the cycle returns **above what it took**.
///
/// The first `assemble` used `expected_output` as the gross itself, which made
/// every candidate look profitable by roughly its own size — a 1-WETH cycle
/// returning 1.002 WETH read as a 1.002-WETH profit. Every other test here builds
/// a `Refinement` by hand with a small `expected_output`, so the join the plane
/// actually runs was never exercised; this one runs it.
#[tokio::test]
async fn the_gross_is_what_the_cycle_returns_above_what_it_took() {
    let econ = econ_over(200, L1FeeModel::unvalidated());
    let p = proposal(2);
    let r = apex_runtime::workers::refine_concurrently(&econ, &p).await.expect("refines");
    let (output, input) = (r.expected_output, r.input_amount.get());
    assert!(output > input, "the fixture spread is profitable");
    let c = econ.assemble(&p, r).expect("assembles");

    assert_eq!(c.gross_profit, output - input, "gross is output minus input");
    assert!(
        c.gross_profit < input / AlloyU256::from(10u64),
        "a 2% spread cannot gross 10% of the size: {} on {}",
        c.gross_profit,
        input
    );
    assert!(c.expected_net_profit < i128::try_from(output - input).unwrap());
}
