//! Scenario-conditioned expected value (§2, §14.1).
//!
//! # What this replaces, and why the old form was wrong
//!
//! The legacy shape was
//! `EV = P_land · P_state · P_exec · P_net − C_failure`. Multiplying
//! probabilities asserts they are **independent**, and these are not: the same
//! competitor that takes the pool before us is the reason the state changed, is
//! the reason execution reverts, and is the reason the net came in under
//! forecast. Treating those as four independent coin flips multiplies four
//! numbers that mostly describe one event, and the product is far smaller than
//! the truth in the benign case and far larger in the adverse one.
//!
//! `J(a|I) = Σ_s P(s|I,a)·Π(a,s) − C_irrecoverable(a)` instead enumerates
//! mutually exclusive worlds and prices each. Correlation is expressed by which
//! scenarios exist and what probability they carry, not by a product.
//!
//! # The priors are not measured
//!
//! §14.1: until the competitor model exists (Phase 9) the scenario set is the
//! conservative subset with **pessimistic fixed priors**, flagged
//! `prior=unmeasured`. That flag is a value on [`ScenarioSet`], not a comment,
//! and [`ScenarioSet::is_measured`] is false for everything this phase can
//! build.

use ethers_core::types::U256;

/// §14.1's twelve worlds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScenarioKind {
    SameStateImmediate,
    SameStateOneFlashblockLater,
    CompetingSamePoolSwapFirst,
    CompetingArbitrageFirst,
    TargetBackrunStateChanged,
    DelayedBeyondValidity,
    RouteExecutionSuccess,
    VenueRevert,
    FlashLiquidityFailure,
    CostAboveForecast,
    InclusionRejected,
    PreconfirmationDivergence,
}

impl ScenarioKind {
    pub const ALL: [Self; 12] = [
        Self::SameStateImmediate,
        Self::SameStateOneFlashblockLater,
        Self::CompetingSamePoolSwapFirst,
        Self::CompetingArbitrageFirst,
        Self::TargetBackrunStateChanged,
        Self::DelayedBeyondValidity,
        Self::RouteExecutionSuccess,
        Self::VenueRevert,
        Self::FlashLiquidityFailure,
        Self::CostAboveForecast,
        Self::InclusionRejected,
        Self::PreconfirmationDivergence,
    ];

    /// The conservative Phase-3 subset (§14.1). Everything else needs the
    /// competitor model that arrives in Phase 9.
    pub const PHASE_3_SUBSET: [Self; 4] = [
        Self::SameStateImmediate,
        Self::SameStateOneFlashblockLater,
        Self::CompetingSamePoolSwapFirst,
        Self::VenueRevert,
    ];
}

/// Profit in one world. Signed: most of these worlds lose money.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profit(pub i128);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scenario {
    pub kind: ScenarioKind,
    /// Parts per million. Integer, because a probability that multiplies a wei
    /// quantity must not bring floating point into the money.
    pub probability_ppm: u32,
    pub profit: Profit,
}

/// Where a scenario set's probabilities came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriorSource {
    /// Pessimistic fixed values from configuration. §14.1's Phase-3 state.
    Unmeasured,
    /// From the Phase-9 competitor model, over a recorded sample.
    Measured { samples: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScenarioSet {
    pub scenarios: Vec<Scenario>,
    pub prior: PriorSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScenarioError {
    /// Probabilities do not sum to one, within tolerance.
    ///
    /// Not rounded away: a set summing to 0.6 is missing 40% of the outcome
    /// space, and the missing 40% is exactly where the unmodelled disasters
    /// live. Silently normalising would redistribute that mass across the
    /// scenarios somebody *did* think of.
    ProbabilitiesDoNotSumToOne { total_ppm: u64 },
    /// The same world listed twice. Scenarios are mutually exclusive by
    /// construction; a duplicate means the partition is wrong.
    DuplicateScenario(ScenarioKind),
    Empty,
}

impl ScenarioSet {
    pub const fn is_measured(&self) -> bool {
        matches!(self.prior, PriorSource::Measured { .. })
    }

    /// Probabilities must partition the outcome space. Tolerance is one ppm
    /// per scenario, to absorb the rounding of a config file's decimals and
    /// nothing more.
    pub fn validate(&self) -> Result<(), ScenarioError> {
        if self.scenarios.is_empty() {
            return Err(ScenarioError::Empty);
        }
        let mut seen: Vec<ScenarioKind> = Vec::new();
        for s in &self.scenarios {
            if seen.contains(&s.kind) {
                return Err(ScenarioError::DuplicateScenario(s.kind));
            }
            seen.push(s.kind);
        }
        let total: u64 = self.scenarios.iter().map(|s| u64::from(s.probability_ppm)).sum();
        let tolerance = self.scenarios.len() as u64;
        if total.abs_diff(1_000_000) > tolerance {
            return Err(ScenarioError::ProbabilitiesDoNotSumToOne { total_ppm: total });
        }
        Ok(())
    }
}

/// `J(a|I) = Σ_s P(s|I,a)·Π(a,s) − C_irrecoverable(a)`.
///
/// `c_irrecoverable` is what is spent no matter which world occurs — the L1
/// data fee of a transaction that was included and reverted is the canonical
/// example. It sits **outside** the sum because it is not conditional on a
/// scenario; folding it into each scenario's profit would make it look like
/// something the scenario distribution could avoid.
pub fn scenario_ev(set: &ScenarioSet, c_irrecoverable: U256) -> Result<i128, ScenarioError> {
    set.validate()?;
    let weighted: i128 = set
        .scenarios
        .iter()
        .map(|s| {
            s.profit
                .0
                .saturating_mul(i128::from(s.probability_ppm))
                .saturating_div(1_000_000)
        })
        .fold(0i128, i128::saturating_add);
    let irrecoverable = i128::try_from(c_irrecoverable.min(U256::from(i128::MAX as u128)).as_u128())
        .unwrap_or(i128::MAX);
    Ok(weighted.saturating_sub(irrecoverable))
}

/// The probability the trade makes money at all: `Pr(Π(a,s) > 0)`, in ppm.
///
/// §2.1's robust gate needs this separately from `J`, because a positive
/// expectation carried by one rare enormous win is not the same trade as a
/// positive expectation that usually pays.
pub fn probability_of_profit_ppm(set: &ScenarioSet) -> Result<u32, ScenarioError> {
    set.validate()?;
    Ok(set
        .scenarios
        .iter()
        .filter(|s| s.profit.0 > 0)
        .map(|s| s.probability_ppm)
        .fold(0u32, u32::saturating_add))
}

/// The naive independent-product form, for comparison only.
///
/// Present so the difference between the two can be *measured* rather than
/// asserted. Nothing in the engine may call this to make a decision.
#[doc(hidden)]
pub fn naive_independent_ev(
    p_land_ppm: u32,
    p_state_ppm: u32,
    p_exec_ppm: u32,
    p_net_ppm: u32,
    profit: i128,
    c_failure: i128,
) -> i128 {
    let p = u128::from(p_land_ppm)
        * u128::from(p_state_ppm)
        * u128::from(p_exec_ppm)
        * u128::from(p_net_ppm);
    let scaled = profit.saturating_mul(p as i128) / 1_000_000_000_000_000_000_000_000i128;
    scaled.saturating_sub(c_failure)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(kind: ScenarioKind, ppm: u32, profit: i128) -> Scenario {
        Scenario {
            kind,
            probability_ppm: ppm,
            profit: Profit(profit),
        }
    }

    /// The Phase-3 subset, with pessimistic priors: the trade lands cleanly
    /// less than half the time.
    fn conservative() -> ScenarioSet {
        ScenarioSet {
            scenarios: vec![
                s(ScenarioKind::SameStateImmediate, 400_000, 1_000_000),
                s(ScenarioKind::SameStateOneFlashblockLater, 250_000, 600_000),
                s(ScenarioKind::CompetingSamePoolSwapFirst, 250_000, -50_000),
                s(ScenarioKind::VenueRevert, 100_000, -120_000),
            ],
            prior: PriorSource::Unmeasured,
        }
    }

    #[test]
    fn the_phase_three_set_is_the_conservative_subset_and_says_it_is_unmeasured() {
        let set = conservative();
        assert!(set.validate().is_ok());
        assert!(!set.is_measured(), "Phase 3 priors are configured, not measured");
        for s in &set.scenarios {
            assert!(
                ScenarioKind::PHASE_3_SUBSET.contains(&s.kind),
                "{:?} needs the Phase 9 competitor model",
                s.kind
            );
        }
    }

    /// The headline claim: `J` is not the product of independent
    /// probabilities. Measured, not asserted — the two differ by more than a
    /// rounding.
    #[test]
    fn scenario_ev_is_not_a_product_of_independent_probabilities() {
        let set = conservative();
        let j = scenario_ev(&set, U256::from(20_000u64)).expect("valid");

        // The same beliefs expressed the old way: land 65% (the two benign
        // worlds), state holds 75%, execution succeeds 90%, net as forecast.
        let naive = naive_independent_ev(650_000, 750_000, 900_000, 1_000_000, 1_000_000, 20_000);

        assert_ne!(j, naive);

        // Measured on this fixture: J = 505,500 against a naive 418,750. The
        // naive form understates by 86,750, which is **17% of the expected
        // value** -- not a rounding artefact, and enough to move a marginal
        // trade across the admission line in either direction.
        //
        // The direction here is pessimistic: multiplying four probabilities
        // below one compounds, so the product understates a benign case. Under
        // correlated failure the same form OVERSTATES, because the four
        // factors are then mostly re-describing one event. That asymmetry is
        // the reason a product cannot be repaired by tuning its inputs.
        //
        // The threshold is 10% rather than the measured 17% so that a fixture
        // tweak does not silently gut the test, and is anchored to a
        // measurement rather than to taste.
        let gap = (j - naive).abs();
        assert!(
            gap * 10 > j.abs(),
            "J = {j}, naive = {naive}: a gap of {gap} is under a tenth of the \
             expected value, so this fixture no longer demonstrates the point"
        );
    }

    /// Probabilities that do not partition the space are rejected, not
    /// normalised. The missing mass is where the unmodelled disasters live.
    #[test]
    fn a_partial_scenario_set_is_refused_rather_than_normalised() {
        let mut set = conservative();
        set.scenarios.pop(); // drop VenueRevert: 10% of the space vanishes
        assert_eq!(
            set.validate(),
            Err(ScenarioError::ProbabilitiesDoNotSumToOne { total_ppm: 900_000 })
        );
        assert!(scenario_ev(&set, U256::zero()).is_err());
    }

    #[test]
    fn a_duplicated_world_is_refused() {
        let mut set = conservative();
        set.scenarios[1].kind = ScenarioKind::SameStateImmediate;
        assert_eq!(
            set.validate(),
            Err(ScenarioError::DuplicateScenario(ScenarioKind::SameStateImmediate))
        );
    }

    /// The irrecoverable cost sits outside the sum, so it is not something the
    /// scenario distribution can dodge.
    #[test]
    fn the_irrecoverable_cost_is_not_conditional_on_a_scenario() {
        let set = conservative();
        let free = scenario_ev(&set, U256::zero()).expect("valid");
        let costly = scenario_ev(&set, U256::from(100_000u64)).expect("valid");
        assert_eq!(free - costly, 100_000, "the cost must subtract in full");
    }

    /// A positive expectation carried by one rare win is a different trade
    /// from one that usually pays, and §2.1 needs to tell them apart.
    #[test]
    fn probability_of_profit_is_separate_from_expectation() {
        let lottery = ScenarioSet {
            scenarios: vec![
                s(ScenarioKind::SameStateImmediate, 10_000, 1_000_000_000),
                s(ScenarioKind::CompetingSamePoolSwapFirst, 990_000, -5_000),
            ],
            prior: PriorSource::Unmeasured,
        };
        let j = scenario_ev(&lottery, U256::zero()).expect("valid");
        assert!(j > 0, "the lottery has a positive expectation: {j}");
        assert_eq!(
            probability_of_profit_ppm(&lottery).expect("valid"),
            10_000,
            "...and pays one time in a hundred"
        );
        assert!(
            probability_of_profit_ppm(&conservative()).expect("valid") > 600_000,
            "the conservative set usually pays"
        );
    }
}
