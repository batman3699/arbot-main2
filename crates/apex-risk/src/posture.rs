//! The graduated response ladder (§25.2, §28.1). **INV-42.**
//!
//! > A risk trigger produces a graduated response, **never an unhandled
//! > continue**.
//!
//! Which is why [`RiskTrigger::minimum_posture`] is a total function over an
//! exhaustive enum and returns a [`RiskPosture`] rather than an `Option`. There
//! is no path through this module that observes a trigger and does nothing, and
//! adding a fifteenth trigger without deciding its posture is a compile error.
//!
//! # The ladder only goes up on its own
//!
//! §25.2: "transitions are automatic on trigger and require an explicit
//! operator action (or a measured recovery window) to step back down."
//! [`PostureLadder::observe`] takes the max; stepping down needs
//! [`StepDownAuthority`], which cannot be constructed by accident.

use apex_types::risk::RiskPosture;
use apex_types::time::{DurationNanos, UnixNanos};

/// §25.1's fourteen triggers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RiskTrigger {
    StateStaleness,
    PreconfDivergence,
    SimulationDivergence,
    RevertSpike,
    UnexpectedCallback,
    VenueInvariantViolation,
    ProfitShortfall,
    FeeAnomaly,
    GasAnomaly,
    CompetitorIntensityAnomaly,
    SequencerBuilderAcceptanceCollapse,
    NodeDesynchronization,
    ContractCodeFingerprintChange,
    FlashSourceReliabilityCollapse,
}

impl RiskTrigger {
    pub const ALL: [Self; 14] = [
        Self::StateStaleness,
        Self::PreconfDivergence,
        Self::SimulationDivergence,
        Self::RevertSpike,
        Self::UnexpectedCallback,
        Self::VenueInvariantViolation,
        Self::ProfitShortfall,
        Self::FeeAnomaly,
        Self::GasAnomaly,
        Self::CompetitorIntensityAnomaly,
        Self::SequencerBuilderAcceptanceCollapse,
        Self::NodeDesynchronization,
        Self::ContractCodeFingerprintChange,
        Self::FlashSourceReliabilityCollapse,
    ];

    /// The floor this trigger puts under the posture.
    ///
    /// **These assignments are judgement, and §28.1 does not enumerate them.**
    /// They live in one `match` so they can be argued with. The reasoning:
    ///
    /// - *Size reductions* are for conditions where the model is still right
    ///   but noisier — stale state we can partly reconstruct, a revert spike, a
    ///   shortfall against expectation.
    /// - *High-EV only* is for conditions that raise the cost of being wrong
    ///   without making us wrong — fee and gas anomalies, a crowded block, a
    ///   preconfirmation we cannot match.
    /// - *Strategy disabled* is for conditions where **our model is wrong**: a
    ///   simulation that disagreed with the chain, a venue whose invariant did
    ///   not hold, a flash source that stopped being a source.
    /// - *Chain disabled* is for conditions where the **chain's** acceptance
    ///   path is gone — no sequencer, no builder, a desynchronized node. There
    ///   is nothing to trade against and nothing a smaller size fixes.
    /// - *Global halt* is for the two that mean **something else is driving**:
    ///   an unexpected callback (somebody is calling our executor — INV-33) and
    ///   a contract code fingerprint change (the contract under us is not the
    ///   one we audited). Neither is a market condition, and neither is safe on
    ///   any other chain either until it is understood.
    pub const fn minimum_posture(self) -> RiskPosture {
        match self {
            Self::StateStaleness | Self::RevertSpike | Self::ProfitShortfall => {
                RiskPosture::ReducedSize
            }
            Self::PreconfDivergence
            | Self::FeeAnomaly
            | Self::GasAnomaly
            | Self::CompetitorIntensityAnomaly => RiskPosture::HighEvOnly,
            Self::SimulationDivergence
            | Self::VenueInvariantViolation
            | Self::FlashSourceReliabilityCollapse => RiskPosture::StrategyDisabled,
            Self::SequencerBuilderAcceptanceCollapse | Self::NodeDesynchronization => {
                RiskPosture::ChainDisabled
            }
            Self::UnexpectedCallback | Self::ContractCodeFingerprintChange => {
                RiskPosture::GlobalHalt
            }
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::StateStaleness => "state_staleness",
            Self::PreconfDivergence => "preconf_divergence",
            Self::SimulationDivergence => "simulation_divergence",
            Self::RevertSpike => "revert_spike",
            Self::UnexpectedCallback => "unexpected_callback",
            Self::VenueInvariantViolation => "venue_invariant_violation",
            Self::ProfitShortfall => "profit_shortfall",
            Self::FeeAnomaly => "fee_anomaly",
            Self::GasAnomaly => "gas_anomaly",
            Self::CompetitorIntensityAnomaly => "competitor_intensity_anomaly",
            Self::SequencerBuilderAcceptanceCollapse => "sequencer_builder_acceptance_collapse",
            Self::NodeDesynchronization => "node_desynchronization",
            Self::ContractCodeFingerprintChange => "contract_code_fingerprint_change",
            Self::FlashSourceReliabilityCollapse => "flash_source_reliability_collapse",
        }
    }
}

impl std::fmt::Display for RiskTrigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Permission to step the ladder down. §25.2 allows two sources and this type
/// has exactly two constructors, so a third way of relaxing risk cannot appear
/// without appearing here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepDownAuthority {
    /// A human said so. Carries who, because "who relaxed the risk gate" is the
    /// first question asked after a loss.
    Operator { actor: &'static str },
    /// A measured recovery window elapsed with no further trigger.
    RecoveryWindow { elapsed: DurationNanos },
}

/// Why the ladder refused to step down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepDownRefused {
    /// Already at the bottom.
    AlreadyNormal,
    /// A trigger fired inside the recovery window, so the window did not
    /// actually pass quietly.
    WindowNotQuiet { since_last_trigger: DurationNanos, required: DurationNanos },
    /// A posture only an operator may leave. The two `GlobalHalt` triggers mean
    /// something other than the market is driving, and no elapsed time is
    /// evidence that it stopped.
    RequiresOperator(RiskPosture),
}

impl std::fmt::Display for StepDownRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyNormal => f.write_str("already at Normal"),
            Self::WindowNotQuiet { since_last_trigger, required } => write!(
                f,
                "last trigger was {} ns ago; {} ns of quiet are required",
                since_last_trigger.0, required.0
            ),
            Self::RequiresOperator(p) => write!(f, "{p:?} requires an explicit operator action"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PostureLadder {
    posture: RiskPosture,
    last_trigger: Option<(RiskTrigger, UnixNanos)>,
    /// How much quiet a `RecoveryWindow` step-down needs.
    recovery_window: DurationNanos,
}

impl PostureLadder {
    /// Ten minutes of quiet. Long enough that a burst of one condition does not
    /// look like two quiet periods; short enough that a transient does not cost
    /// an hour of trading.
    pub const DEFAULT_RECOVERY_WINDOW: DurationNanos = DurationNanos(600_000_000_000);

    pub const fn new() -> Self {
        Self {
            posture: RiskPosture::Normal,
            last_trigger: None,
            recovery_window: Self::DEFAULT_RECOVERY_WINDOW,
        }
    }

    pub const fn with_recovery_window(window: DurationNanos) -> Self {
        Self { posture: RiskPosture::Normal, last_trigger: None, recovery_window: window }
    }

    pub const fn posture(&self) -> RiskPosture {
        self.posture
    }

    pub const fn permits_new_live_tickets(&self) -> bool {
        self.posture.permits_new_live_tickets()
    }

    pub const fn last_trigger(&self) -> Option<(RiskTrigger, UnixNanos)> {
        self.last_trigger
    }

    /// **INV-42.** Returns the posture after the trigger — never `None`, never
    /// a silent continue. Takes the max, so a mild trigger cannot relax a
    /// posture a severe one established.
    pub fn observe(&mut self, trigger: RiskTrigger, at: UnixNanos) -> RiskPosture {
        let required = trigger.minimum_posture();
        if required > self.posture {
            self.posture = required;
        }
        self.last_trigger = Some((trigger, at));
        self.posture
    }

    /// One rung down, with authority.
    pub fn step_down(
        &mut self,
        authority: StepDownAuthority,
        now: UnixNanos,
    ) -> Result<RiskPosture, StepDownRefused> {
        if self.posture == RiskPosture::Normal {
            return Err(StepDownRefused::AlreadyNormal);
        }
        if let StepDownAuthority::RecoveryWindow { .. } = authority {
            // A halt is not a market condition. No amount of elapsed time is
            // evidence that whatever was driving has stopped.
            if self.posture == RiskPosture::GlobalHalt {
                return Err(StepDownRefused::RequiresOperator(self.posture));
            }
            if let Some((_, at)) = self.last_trigger {
                let quiet = DurationNanos(now.0.saturating_sub(at.0));
                if quiet < self.recovery_window {
                    return Err(StepDownRefused::WindowNotQuiet {
                        since_last_trigger: quiet,
                        required: self.recovery_window,
                    });
                }
            }
        }
        self.posture = one_rung_down(self.posture);
        Ok(self.posture)
    }
}

impl Default for PostureLadder {
    fn default() -> Self {
        Self::new()
    }
}

/// §25.2's ladder, downward. Written out rather than derived from the variant
/// index so that reordering `RiskPosture` cannot silently reroute it.
const fn one_rung_down(p: RiskPosture) -> RiskPosture {
    match p {
        RiskPosture::Normal | RiskPosture::ReducedSize => RiskPosture::Normal,
        RiskPosture::HighEvOnly => RiskPosture::ReducedSize,
        RiskPosture::StrategyDisabled => RiskPosture::HighEvOnly,
        RiskPosture::ChainDisabled => RiskPosture::StrategyDisabled,
        RiskPosture::GlobalHalt => RiskPosture::ChainDisabled,
    }
}
