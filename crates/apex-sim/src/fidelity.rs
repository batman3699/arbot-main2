//! Simulation fidelity scoring (§34, INV-44).
//!
//! Compares what simulation predicted against what the chain did, per
//! strategy × venue, and escalates a response when the two stop agreeing.
//!
//! # Under-prediction and over-prediction are not the same error
//!
//! The obvious band is symmetric: `|realized − predicted| / predicted`. It is
//! the wrong shape.
//!
//! **Over-predicting gas costs opportunity.** The candidate was priced as more
//! expensive than it turned out to be, so some trades were declined that would
//! have paid. That is a loss, and it is bounded by the trades not taken.
//!
//! **Under-predicting gas costs money already committed.** The candidate was
//! priced as cheaper than it is; it was admitted on arithmetic that was wrong
//! in the profitable direction, the gas limit may be short, and the
//! transaction can burn the whole limit and revert. The loss is unbounded by
//! anything the model knew.
//!
//! So [`FidelityPolicy`] carries two tolerances, and the under-prediction one
//! is tighter. A symmetric band treats a dangerous error and a merely wasteful
//! one as the same event.
//!
//! # One bad sample is not a breach
//!
//! A single outlier — an unusual pool state, a competitor's transaction
//! landing between simulation and inclusion — is noise. Escalating on it would
//! disable a working strategy on the first unlucky block. The scorer requires
//! a **minimum sample count** and a **breach rate** over a window, the same
//! shape as `apex_venues::breaker`.
//!
//! # The ladder only climbs on evidence, and only descends on evidence
//!
//! `Normal → ReduceSize → RaiseTier → Disable`. Each step needs its own
//! sustained breach; the scorer cannot skip a rung because a sample was very
//! bad, and it cannot fall back to `Normal` because a window happened to be
//! quiet — recovery needs as many clean samples as the breach needed dirty
//! ones. A ladder that descends faster than it climbs oscillates, and an
//! oscillating gate is one nobody trusts.

use apex_types::ids::{StrategyId, VenueId};
use std::collections::HashMap;
use std::collections::VecDeque;

/// §34's graduated response. `Ord` by severity: the worst response across
/// several signals is the one that applies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FidelityResponse {
    /// No evidence of a problem. The default, because absence of evidence is
    /// not evidence of a problem.
    #[default]
    Normal,
    ReduceSize,
    RaiseTier,
    Disable,
}

impl FidelityResponse {
    /// The next rung up. `Disable` is the top.
    pub const fn escalate(self) -> Self {
        match self {
            Self::Normal => Self::ReduceSize,
            Self::ReduceSize => Self::RaiseTier,
            Self::RaiseTier | Self::Disable => Self::Disable,
        }
    }

    /// The next rung down. `Normal` is the floor.
    pub const fn relax(self) -> Self {
        match self {
            Self::Disable => Self::RaiseTier,
            Self::RaiseTier => Self::ReduceSize,
            Self::ReduceSize | Self::Normal => Self::Normal,
        }
    }

    pub const fn permits_live_dispatch(self) -> bool {
        !matches!(self, Self::Disable)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FidelityPolicy {
    /// How far realized gas may exceed predicted, in bps. Tighter, because
    /// this is the direction that commits money on wrong arithmetic.
    pub max_under_prediction_bps: u32,
    /// How far predicted may exceed realized, in bps. Looser: the cost is
    /// opportunity, not loss.
    pub max_over_prediction_bps: u32,
    /// Observations before any verdict. Below this the answer is `Normal`,
    /// because there is not enough evidence to say otherwise.
    pub min_samples: u32,
    /// Window length.
    pub window: usize,
    /// Breaches in the window, as a fraction in bps, that escalate one rung.
    pub breach_rate_bps: u32,
}

impl Default for FidelityPolicy {
    fn default() -> Self {
        Self {
            max_under_prediction_bps: 500,  // 5%
            max_over_prediction_bps: 2_000, // 20%
            min_samples: 20,
            window: 100,
            breach_rate_bps: 2_000, // a fifth of the window
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    pub predicted_gas: u64,
    pub realized_gas: u64,
}

impl Observation {
    /// Signed error in bps: positive when the chain used MORE than predicted.
    pub fn error_bps(&self) -> i64 {
        if self.predicted_gas == 0 {
            // A prediction of zero is not a prediction. Treated as the worst
            // under-prediction rather than as a division to avoid.
            return i64::MAX;
        }
        let predicted = self.predicted_gas as i64;
        let realized = self.realized_gas as i64;
        (realized - predicted).saturating_mul(10_000) / predicted
    }

    fn breaches(&self, policy: &FidelityPolicy) -> bool {
        let e = self.error_bps();
        if e >= 0 {
            e > i64::from(policy.max_under_prediction_bps)
        } else {
            -e > i64::from(policy.max_over_prediction_bps)
        }
    }
}

#[derive(Debug, Default)]
struct Series {
    window: VecDeque<bool>,
    total: u32,
    response: FidelityResponse,
    /// Samples since the last rung change, so a rung is held for at least one
    /// full window before it can move again.
    since_change: u32,
}

/// Per strategy × venue fidelity, and the response it earns.
#[derive(Debug, Default)]
pub struct FidelityScorer {
    policy: FidelityPolicy,
    series: HashMap<(StrategyId, VenueId), Series>,
}

impl FidelityScorer {
    pub fn new(policy: FidelityPolicy) -> Self {
        Self {
            policy,
            series: HashMap::new(),
        }
    }

    pub const fn policy(&self) -> FidelityPolicy {
        self.policy
    }

    /// The response currently in force. `Normal` for a key never observed —
    /// absence of evidence is not evidence of a problem.
    pub fn response(&self, strategy: StrategyId, venue: VenueId) -> FidelityResponse {
        self.series
            .get(&(strategy, venue))
            .map_or(FidelityResponse::Normal, |s| s.response)
    }

    /// Record one (predicted, realized) pair and return the response now in
    /// force for that strategy × venue.
    pub fn observe(
        &mut self,
        strategy: StrategyId,
        venue: VenueId,
        observation: Observation,
    ) -> FidelityResponse {
        let policy = self.policy;
        let series = self.series.entry((strategy, venue)).or_default();

        series.window.push_back(observation.breaches(&policy));
        if series.window.len() > policy.window {
            series.window.pop_front();
        }
        series.total = series.total.saturating_add(1);
        series.since_change = series.since_change.saturating_add(1);

        if series.total < policy.min_samples {
            return series.response;
        }
        // A rung is held for at least `min_samples` observations after a
        // change, so one window cannot drive two steps.
        if series.since_change < policy.min_samples {
            return series.response;
        }

        let breaches = series.window.iter().filter(|b| **b).count() as u64;
        let rate_bps = (breaches * 10_000 / series.window.len() as u64) as u32;

        if rate_bps > policy.breach_rate_bps {
            let next = series.response.escalate();
            if next != series.response {
                series.response = next;
                series.since_change = 0;
            }
        } else if breaches == 0 {
            // Recovery needs a clean window, not merely a below-threshold one:
            // a ladder that descends faster than it climbs oscillates.
            let next = series.response.relax();
            if next != series.response {
                series.response = next;
                series.since_change = 0;
            }
        }
        series.response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: StrategyId = StrategyId(1);
    const V: VenueId = VenueId(1);

    fn under(bps: u64) -> Observation {
        Observation {
            predicted_gas: 200_000,
            realized_gas: 200_000 + 200_000 * bps / 10_000,
        }
    }

    fn accurate() -> Observation {
        Observation {
            predicted_gas: 200_000,
            realized_gas: 200_000,
        }
    }

    /// Task 4.4's requirement, in order.
    #[test]
    fn sustained_gas_error_drives_reduce_then_raise_then_disable() {
        let mut scorer = FidelityScorer::new(FidelityPolicy::default());
        let mut seen = Vec::new();
        for _ in 0..200 {
            let r = scorer.observe(S, V, under(3_000)); // 30% over, well past 5%
            if seen.last() != Some(&r) {
                seen.push(r);
            }
        }
        assert_eq!(
            seen,
            vec![
                FidelityResponse::Normal,
                FidelityResponse::ReduceSize,
                FidelityResponse::RaiseTier,
                FidelityResponse::Disable
            ],
            "the ladder must climb one rung at a time, in order"
        );
        assert!(!scorer.response(S, V).permits_live_dispatch());
    }

    /// Under-prediction is held to a tighter band than over-prediction,
    /// because it commits money on wrong arithmetic rather than merely
    /// declining trades.
    #[test]
    fn under_prediction_is_judged_more_harshly_than_over_prediction() {
        let policy = FidelityPolicy::default();
        assert!(policy.max_under_prediction_bps < policy.max_over_prediction_bps);

        // 10% over budget: a breach.
        let too_much = Observation {
            predicted_gas: 200_000,
            realized_gas: 220_000,
        };
        assert!(too_much.breaches(&policy));

        // 10% under budget: wasteful, not dangerous, and within band.
        let too_little = Observation {
            predicted_gas: 200_000,
            realized_gas: 180_000,
        };
        assert!(!too_little.breaches(&policy));
    }

    /// One bad sample is noise. Escalating on it would disable a working
    /// strategy on the first unlucky block.
    #[test]
    fn a_single_outlier_does_not_move_the_ladder() {
        let mut scorer = FidelityScorer::new(FidelityPolicy::default());
        assert_eq!(scorer.observe(S, V, under(50_000)), FidelityResponse::Normal);
        for _ in 0..50 {
            scorer.observe(S, V, accurate());
        }
        assert_eq!(scorer.response(S, V), FidelityResponse::Normal);
    }

    /// Each strategy × venue is scored independently. One venue's bad
    /// simulation must not disable another's.
    #[test]
    fn one_key_degrading_does_not_touch_another() {
        let mut scorer = FidelityScorer::new(FidelityPolicy::default());
        let other = VenueId(2);
        for _ in 0..200 {
            scorer.observe(S, V, under(3_000));
        }
        assert_eq!(scorer.response(S, V), FidelityResponse::Disable);
        assert_eq!(scorer.response(S, other), FidelityResponse::Normal);
        assert_eq!(scorer.response(StrategyId(2), V), FidelityResponse::Normal);
    }

    /// Recovery descends the ladder, one rung at a time, and needs a clean
    /// window rather than a merely quiet one.
    #[test]
    fn recovery_descends_one_rung_at_a_time() {
        let mut scorer = FidelityScorer::new(FidelityPolicy::default());
        for _ in 0..200 {
            scorer.observe(S, V, under(3_000));
        }
        assert_eq!(scorer.response(S, V), FidelityResponse::Disable);

        let mut seen = vec![FidelityResponse::Disable];
        for _ in 0..500 {
            let r = scorer.observe(S, V, accurate());
            if seen.last() != Some(&r) {
                seen.push(r);
            }
        }
        assert_eq!(
            seen,
            vec![
                FidelityResponse::Disable,
                FidelityResponse::RaiseTier,
                FidelityResponse::ReduceSize,
                FidelityResponse::Normal
            ],
            "recovery must descend one rung at a time"
        );
    }

    /// A prediction of zero is not a prediction, and is treated as the worst
    /// under-prediction rather than as a division to sidestep.
    #[test]
    fn a_zero_prediction_is_the_worst_kind_of_error() {
        let o = Observation {
            predicted_gas: 0,
            realized_gas: 100_000,
        };
        assert_eq!(o.error_bps(), i64::MAX);
        assert!(o.breaches(&FidelityPolicy::default()));
    }

    /// Below the minimum sample count the answer is `Normal`: absence of
    /// evidence is not evidence of a problem.
    #[test]
    fn a_short_history_does_not_convict() {
        let mut scorer = FidelityScorer::new(FidelityPolicy::default());
        for _ in 0..(FidelityPolicy::default().min_samples - 1) {
            assert_eq!(scorer.observe(S, V, under(9_000)), FidelityResponse::Normal);
        }
    }

    /// The error sign is right: more gas than predicted is positive.
    #[test]
    fn the_error_sign_says_which_way_the_model_was_wrong() {
        assert_eq!(
            Observation {
                predicted_gas: 200_000,
                realized_gas: 220_000
            }
            .error_bps(),
            1_000
        );
        assert_eq!(
            Observation {
                predicted_gas: 200_000,
                realized_gas: 180_000
            }
            .error_bps(),
            -1_000
        );
    }
}
