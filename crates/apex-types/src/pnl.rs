//! Realized P&L attribution (Blueprint §32).

use crate::cost::TotalExecutionCost;
use crate::ids::{ChainId, StrategyId, TicketId, VenueId};
use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

/// §32 "Attribution": incremental P&L must be attributable to each optimization
/// layer, because §1.4 disables any module that cannot show incremental P&L.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OptimizationLayer {
    SinglePath,
    ParallelSplit,
    JointAllocation,
    CrossCyclePacking,
    EventDriven,
    Liquidation,
    Correlated,
    V4Route,
    ChainSubmissionOpt,
    ComputeScheduling,
}

impl OptimizationLayer {
    pub const ALL: [Self; 10] = [
        Self::SinglePath,
        Self::ParallelSplit,
        Self::JointAllocation,
        Self::CrossCyclePacking,
        Self::EventDriven,
        Self::Liquidation,
        Self::Correlated,
        Self::V4Route,
        Self::ChainSubmissionOpt,
        Self::ComputeScheduling,
    ];
}

/// §2.10: USD is a bounded interval, never a point estimate.
///
/// A candidate must stay economically valid under the conservative bound, and a
/// USD mark may never admit a trade on its own (INV-20). Modelling it as an
/// interval makes "which end did you use?" impossible to skip.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsdBounds {
    pub low: f64,
    pub high: f64,
}

impl UsdBounds {
    /// The only value the risk gate may read. Named so that reaching for the
    /// optimistic end is a deliberate act rather than a field access.
    pub const fn conservative(&self) -> f64 {
        self.low
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PnlAttribution {
    pub ticket_id: TicketId,
    pub chain: ChainId,
    pub strategy: StrategyId,
    pub venues: Vec<VenueId>,
    pub route_hash: B256,
    pub optimization_layers: Vec<OptimizationLayer>,
    pub gross_profit: i128,
    pub realized_cost: TotalExecutionCost,
    /// In units of the profit token. This, not the USD figure, is what decides
    /// whether the trade made money (§2.10).
    pub net_profit_token: i128,
    pub net_profit_usd_bounds: UsdBounds,
}
