//! Risk decisions and the graduated response ladder (Blueprint §28).

use crate::miss::MissReason;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RiskDecision {
    Admit { size_multiplier: f64 },
    AdmitReduced { size_multiplier: f64, reason: String },
    Reject { reason: MissReason, rule: String },
}

impl RiskDecision {
    pub const fn admits(&self) -> bool {
        matches!(self, Self::Admit { .. } | Self::AdmitReduced { .. })
    }
}

/// §28.1. Ordered by severity; `Ord` is used to take the worst of several
/// concurrent triggers, so variant order is load-bearing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RiskPosture {
    Normal,
    ReducedSize,
    HighEvOnly,
    StrategyDisabled,
    ChainDisabled,
    GlobalHalt,
}

impl RiskPosture {
    pub const fn permits_new_live_tickets(self) -> bool {
        !matches!(self, Self::StrategyDisabled | Self::ChainDisabled | Self::GlobalHalt)
    }
}

/// §28.2. Exhaustive: a loss class that exceeds its expected frequency
/// automatically tightens its gate, which is impossible if losses can land in
/// an "other" bucket.
///
/// `Ord` is derived so a class can key a `BTreeMap` and the loss ledger iterates
/// deterministically. **Unlike [`RiskPosture`], this order is NOT severity** --
/// there is no sense in which a pricing error is "less" than a venue error, and
/// nothing may take a max over it. Severity lives in the per-class budget's
/// `on_breach` posture (`apex_risk::loss::ClassBudget`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum LossClass {
    Pricing,
    State,
    Simulation,
    Venue,
    Inclusion,
    FeeModel,
    Contract,
    OperatorConfig,
    ExternalProtocol,
}

impl LossClass {
    pub const ALL: [Self; 9] = [
        Self::Pricing,
        Self::State,
        Self::Simulation,
        Self::Venue,
        Self::Inclusion,
        Self::FeeModel,
        Self::Contract,
        Self::OperatorConfig,
        Self::ExternalProtocol,
    ];
}
