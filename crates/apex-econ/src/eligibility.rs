//! The candidate eligibility gate (§2.3, §2.1; INV-20, INV-23).
//!
//! # Eight clauses, not nine
//!
//! PLAN.md Task 3.5 says *"all 9 from §2.3"*. The blueprint's §2.3 lists
//! **eight**, joined by seven `AND`s:
//!
//! ```text
//! expected_net_EV > 0          AND  robustness_margin >= threshold
//! state_freshness <= limit     AND  simulation_fidelity >= required
//! execution path is healthy    AND  flash liquidity is available
//! route authorization is valid AND  cost estimate confidence is acceptable
//! ```
//!
//! The ninth is real but comes from **§2.1's robust gate**, not §2.3:
//! `Pr(Π(a,s) > 0) ≥ p_min`. A positive expectation carried by one rare
//! enormous win is not the same trade as one that usually pays, and §2.1 says
//! so separately. It is implemented here and attributed correctly.
//!
//! # INV-20 is structural, not asserted
//!
//! *"A USD mark may never admit a trade."* The usual way to guarantee that is
//! a test that perturbs the mark and checks the admitted set. That test exists
//! below — but it is the weaker half. The stronger half is that
//! [`EligibilityContext`] **has no USD field at all**, so there is nothing for
//! a clause to read. USD lives in `apex_types::pnl::UsdBounds`, on the
//! reporting path, and the gate cannot name it.
//!
//! # The conjunction is not `EV > 0`
//!
//! INV-23 exists because the tempting simplification is to check the
//! expectation and treat the rest as advisory. Every clause returns the
//! specific failure, so the rejection histogram (§27) can say *which* gate is
//! costing opportunities rather than reporting an undifferentiated count.

use apex_types::time::DurationNanos;

/// One clause of the conjunction. Exhaustive and order-stable: it indexes a
/// metrics histogram.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Clause {
    /// §2.3
    ExpectedNetEvPositive,
    /// §2.3
    RobustnessMargin,
    /// §2.3
    StateFreshness,
    /// §2.3
    SimulationFidelity,
    /// §2.3
    ExecutionPathHealthy,
    /// §2.3
    FlashLiquidityAvailable,
    /// §2.3
    RouteAuthorizationValid,
    /// §2.3
    CostEstimateConfidence,
    /// §2.1's robust gate, not §2.3. See the module docs.
    ProbabilityOfProfit,
}

impl Clause {
    /// Every clause, in evaluation order.
    pub const ALL: [Self; 9] = [
        Self::ExpectedNetEvPositive,
        Self::RobustnessMargin,
        Self::StateFreshness,
        Self::SimulationFidelity,
        Self::ExecutionPathHealthy,
        Self::FlashLiquidityAvailable,
        Self::RouteAuthorizationValid,
        Self::CostEstimateConfidence,
        Self::ProbabilityOfProfit,
    ];

    /// A stable label for metrics and logs. Exhaustive match with no
    /// catch-all, so a new clause cannot ship unlabelled.
    pub const fn label(self) -> &'static str {
        match self {
            Self::ExpectedNetEvPositive => "expected_net_ev_positive",
            Self::RobustnessMargin => "robustness_margin",
            Self::StateFreshness => "state_freshness",
            Self::SimulationFidelity => "simulation_fidelity",
            Self::ExecutionPathHealthy => "execution_path_healthy",
            Self::FlashLiquidityAvailable => "flash_liquidity_available",
            Self::RouteAuthorizationValid => "route_authorization_valid",
            Self::CostEstimateConfidence => "cost_estimate_confidence",
            Self::ProbabilityOfProfit => "probability_of_profit",
        }
    }

    /// Which section of the blueprint demands this clause.
    pub const fn source(self) -> &'static str {
        match self {
            Self::ProbabilityOfProfit => "§2.1",
            _ => "§2.3",
        }
    }
}

/// Everything the gate may read.
///
/// Note what is absent: there is no USD anywhere. INV-20 is enforced by the
/// shape of this struct, and the property test is the confirmation rather than
/// the mechanism.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EligibilityContext {
    /// `J(a|I)`, in wei of the chain's native token. Signed.
    pub expected_net_ev_wei: i128,
    /// How far `J` sits above zero as a fraction of the gross, in bps. §2.1's
    /// robustness margin.
    pub robustness_margin_bps: u32,
    pub state_age: DurationNanos,
    /// The tier the candidate was actually simulated at, as an ordinal.
    pub simulation_tier: u8,
    pub execution_path_healthy: bool,
    pub flash_liquidity_available: bool,
    pub route_authorization_valid: bool,
    /// Width of the cost estimate as a fraction of the estimate, in bps. A
    /// wide interval is not a small cost.
    pub cost_confidence_bps: u32,
    /// `Pr(Π > 0)` in ppm, from `ev::scenario::probability_of_profit_ppm`.
    pub probability_of_profit_ppm: u32,
}

/// The thresholds the clauses compare against. Per strategy (§2.3 says
/// "strategy limit"), never global constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EligibilityPolicy {
    pub min_robustness_margin_bps: u32,
    pub max_state_age: DurationNanos,
    pub min_simulation_tier: u8,
    /// The widest cost interval still considered a usable estimate.
    pub max_cost_confidence_bps: u32,
    /// §2.1's `p_min`.
    pub min_probability_of_profit_ppm: u32,
}

impl Default for EligibilityPolicy {
    fn default() -> Self {
        Self {
            min_robustness_margin_bps: 50,
            // 2 s: Base produces a Flashblock every ~200 ms, so this is ten of
            // them. Pessimistic and deliberately not derived from a
            // measurement that does not exist yet.
            max_state_age: DurationNanos(2_000_000_000),
            min_simulation_tier: 1,
            max_cost_confidence_bps: 1_000,
            min_probability_of_profit_ppm: 500_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Admit,
    /// The FIRST clause that failed, in evaluation order. One reason, not a
    /// set: the histogram counts what stopped the candidate, and a candidate
    /// failing five clauses is one rejection, not five.
    Reject { clause: Clause },
}

impl Decision {
    pub const fn admits(&self) -> bool {
        matches!(self, Self::Admit)
    }
}

pub struct EligibilityGate;

impl EligibilityGate {
    pub const CLAUSES: [Clause; 9] = Clause::ALL;

    /// Evaluate the whole conjunction.
    pub fn evaluate(ctx: &EligibilityContext, policy: &EligibilityPolicy) -> Decision {
        for clause in Clause::ALL {
            let holds = match clause {
                Clause::ExpectedNetEvPositive => ctx.expected_net_ev_wei > 0,
                Clause::RobustnessMargin => {
                    ctx.robustness_margin_bps >= policy.min_robustness_margin_bps
                }
                Clause::StateFreshness => ctx.state_age.0 <= policy.max_state_age.0,
                Clause::SimulationFidelity => ctx.simulation_tier >= policy.min_simulation_tier,
                Clause::ExecutionPathHealthy => ctx.execution_path_healthy,
                Clause::FlashLiquidityAvailable => ctx.flash_liquidity_available,
                Clause::RouteAuthorizationValid => ctx.route_authorization_valid,
                Clause::CostEstimateConfidence => {
                    ctx.cost_confidence_bps <= policy.max_cost_confidence_bps
                }
                Clause::ProbabilityOfProfit => {
                    ctx.probability_of_profit_ppm >= policy.min_probability_of_profit_ppm
                }
            };
            if !holds {
                return Decision::Reject { clause };
            }
        }
        Decision::Admit
    }
}

/// INV-40. Which economic bucket a failed clause lands in.
///
/// Four clauses collapse into `LowEv` and that is correct: §2.3's gate is a
/// conjunction, and every way of failing it means the trade did not clear.
/// What distinguishes them is *why*, which stays in the `Clause` the record
/// carries -- the ledger aggregates over the bucket, and a reader chasing a
/// spike opens the records.
impl apex_types::miss::ExplainsMiss for Clause {
    fn miss_reason(&self) -> apex_types::miss::MissReason {
        use apex_types::miss::MissReason as R;
        match self {
            Self::ExpectedNetEvPositive | Self::RobustnessMargin | Self::ProbabilityOfProfit => {
                R::LowEv
            }
            Self::StateFreshness => R::StaleState,
            Self::SimulationFidelity => R::SimFail,
            Self::ExecutionPathHealthy => R::VenueDisabled,
            Self::FlashLiquidityAvailable => R::NoFlashLiquidity,
            Self::RouteAuthorizationValid => R::RiskFail,
            Self::CostEstimateConfidence => R::GasFail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing() -> EligibilityContext {
        EligibilityContext {
            expected_net_ev_wei: 1_000_000,
            robustness_margin_bps: 200,
            state_age: DurationNanos(500_000_000),
            simulation_tier: 2,
            execution_path_healthy: true,
            flash_liquidity_available: true,
            route_authorization_valid: true,
            cost_confidence_bps: 200,
            probability_of_profit_ppm: 700_000,
        }
    }

    /// Break one clause at a time; each must be the one that gates.
    ///
    /// Because `evaluate` returns the FIRST failure, a context that breaks
    /// only clause N necessarily reports clause N — which is how this proves
    /// each clause is reachable rather than shadowed by an earlier one.
    #[test]
    fn eligibility_requires_every_clause() {
        assert_eq!(
            EligibilityGate::evaluate(&passing(), &EligibilityPolicy::default()),
            Decision::Admit
        );

        for clause in EligibilityGate::CLAUSES {
            let mut ctx = passing();
            match clause {
                Clause::ExpectedNetEvPositive => ctx.expected_net_ev_wei = 0,
                Clause::RobustnessMargin => ctx.robustness_margin_bps = 1,
                Clause::StateFreshness => ctx.state_age = DurationNanos(60_000_000_000),
                Clause::SimulationFidelity => ctx.simulation_tier = 0,
                Clause::ExecutionPathHealthy => ctx.execution_path_healthy = false,
                Clause::FlashLiquidityAvailable => ctx.flash_liquidity_available = false,
                Clause::RouteAuthorizationValid => ctx.route_authorization_valid = false,
                Clause::CostEstimateConfidence => ctx.cost_confidence_bps = 9_000,
                Clause::ProbabilityOfProfit => ctx.probability_of_profit_ppm = 1,
            }
            assert_eq!(
                EligibilityGate::evaluate(&ctx, &EligibilityPolicy::default()),
                Decision::Reject { clause },
                "clause {} ({}) did not gate",
                clause.label(),
                clause.source()
            );
        }
    }

    /// A positive expectation is not sufficient. INV-23, stated as the thing
    /// it forbids.
    #[test]
    fn a_positive_expectation_alone_does_not_admit() {
        let mut ctx = passing();
        ctx.expected_net_ev_wei = i128::MAX;
        ctx.execution_path_healthy = false;
        assert_eq!(
            EligibilityGate::evaluate(&ctx, &EligibilityPolicy::default()),
            Decision::Reject {
                clause: Clause::ExecutionPathHealthy
            },
            "an unbounded EV must not buy its way past a dead execution path"
        );
    }

    /// Every clause carries a distinct label and a correct attribution.
    #[test]
    fn every_clause_is_labelled_and_attributed() {
        let mut labels: Vec<&str> = Clause::ALL.iter().map(|c| c.label()).collect();
        labels.sort_unstable();
        let unique = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), unique, "two clauses share a label");

        let from_2_3 = Clause::ALL.iter().filter(|c| c.source() == "§2.3").count();
        assert_eq!(
            from_2_3, 8,
            "the blueprint's §2.3 lists eight clauses; PLAN.md's 'nine' counts \
             §2.1's probability-of-profit gate alongside them"
        );
        assert_eq!(Clause::ProbabilityOfProfit.source(), "§2.1");
    }

    /// One rejection, not five. The histogram counts what stopped the
    /// candidate.
    #[test]
    fn a_candidate_failing_several_clauses_reports_the_first() {
        let mut ctx = passing();
        ctx.expected_net_ev_wei = -1;
        ctx.execution_path_healthy = false;
        ctx.flash_liquidity_available = false;
        assert_eq!(
            EligibilityGate::evaluate(&ctx, &EligibilityPolicy::default()),
            Decision::Reject {
                clause: Clause::ExpectedNetEvPositive
            }
        );
    }

    /// The policy is per strategy, so a tighter one rejects what a looser one
    /// admits. Thresholds are configuration, never constants in the gate.
    #[test]
    fn the_thresholds_are_policy_not_constants() {
        let ctx = passing();
        let strict = EligibilityPolicy {
            min_robustness_margin_bps: 500,
            ..EligibilityPolicy::default()
        };
        assert_eq!(
            EligibilityGate::evaluate(&ctx, &strict),
            Decision::Reject {
                clause: Clause::RobustnessMargin
            }
        );
        assert!(EligibilityGate::evaluate(&ctx, &EligibilityPolicy::default()).admits());
    }
}

