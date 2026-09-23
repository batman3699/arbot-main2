//! Final-state reconciliation (§21.5, §2.10). Produces the `PnlAttribution`
//! that the miss ledger and the loss classifier both read.
//!
//! # Realized, not estimated
//!
//! Every figure here comes from the receipt and the balance deltas. Nothing is
//! re-derived: the L1 data fee is **read** from the receipt rather than
//! recomputed with the Fjord formula, because reconciliation records what
//! happened and a recomputation would report the estimator's opinion as the
//! realized cost. Comparing the two is a separate and worthwhile exercise — it
//! is how the estimator gets validated — but it is not this function's job, and
//! doing it here would make the estimator unfalsifiable.
//!
//! # The profit token decides
//!
//! §2.10: "this, not the USD figure, is what decides whether the trade made
//! money". So `net_profit_token` comes from the balance delta of the profit
//! token and the USD bounds are an input the caller supplies from a price
//! source. A reconciliation that invented a price would be quietly deciding
//! profitability with an oracle nobody chose.

use crate::base::observe::Receipt;
use alloy_primitives::Address;
use apex_types::cost::{GasDistribution, GasLimit, GasUsed, TotalExecutionCost};
use apex_types::ids::{ChainId, StrategyId, TicketId, VenueId};
use apex_types::pnl::{OptimizationLayer, PnlAttribution, UsdBounds};
use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

/// One token's movement across the trade, signed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BalanceDelta {
    pub token: Address,
    /// Positive is a gain to us.
    pub delta: i128,
}

/// What the caller knows that the chain does not.
#[derive(Clone, Debug, PartialEq)]
pub struct ReconcileInputs {
    pub ticket: TicketId,
    pub chain: ChainId,
    pub strategy: StrategyId,
    pub venues: Vec<VenueId>,
    pub route_hash: B256,
    pub optimization_layers: Vec<OptimizationLayer>,
    pub profit_token: Address,
    /// The gas limit the transaction was signed with. The receipt reports what
    /// was *used*; INV-19 keeps the two apart.
    pub gas_limit: GasLimit,
    /// From a price source the caller chose. Not invented here.
    pub usd_bounds: UsdBounds,
    /// Fees paid to venues, from the swap logs.
    pub dex_fees_wei: u128,
    /// Flash-loan premium, from the loan event.
    pub flash_fee_wei: u128,
    pub calldata_bytes: u32,
    pub compressed_data_estimate: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileError {
    /// The transaction reverted. There is a P&L — the gas was spent — but it is
    /// a loss with a cause, and `PnlAttribution` is not the type for it:
    /// §28.2's `LossClass` is.
    Reverted { tx: B256, gas_used: u64, spent_wei: u128 },
    /// The profit token never moved. Either the route did not do what it said
    /// or the wrong token was named; neither is a zero-profit trade, and
    /// reporting one would put a false zero in the ledger.
    ProfitTokenAbsent { token: Address },
    /// Two deltas for one token. The caller's accounting is ambiguous and
    /// picking either is a guess.
    DuplicateToken { token: Address },
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reverted { tx, gas_used, spent_wei } => {
                write!(f, "{tx} reverted after {gas_used} gas, costing {spent_wei} wei")
            }
            Self::ProfitTokenAbsent { token } => write!(f, "{token} does not appear in the deltas"),
            Self::DuplicateToken { token } => write!(f, "{token} appears twice in the deltas"),
        }
    }
}

impl std::error::Error for ReconcileError {}

/// §21.5's `reconcile_final_state`.
pub fn reconcile(
    receipt: &Receipt,
    deltas: &[BalanceDelta],
    inputs: &ReconcileInputs,
) -> Result<PnlAttribution, ReconcileError> {
    if !receipt.success {
        return Err(ReconcileError::Reverted {
            tx: receipt.tx,
            gas_used: receipt.gas_used,
            spent_wei: receipt.total_chain_fee_wei(),
        });
    }

    let mut profit_delta = None;
    for (i, d) in deltas.iter().enumerate() {
        if deltas[..i].iter().any(|e| e.token == d.token) {
            return Err(ReconcileError::DuplicateToken { token: d.token });
        }
        if d.token == inputs.profit_token {
            profit_delta = Some(d.delta);
        }
    }
    let Some(gross) = profit_delta else {
        return Err(ReconcileError::ProfitTokenAbsent { token: inputs.profit_token });
    };

    // Realized: every component is what the chain charged or what a log said.
    // The distribution collapses to a point because a realized cost has no
    // spread -- reporting p50 != p99 here would invent uncertainty about
    // something already observed.
    let used = GasUsed(receipt.gas_used);
    let realized_cost = TotalExecutionCost {
        l2_execution_fee: receipt.l2_execution_fee_wei(),
        l1_data_fee: receipt.l1_fee_wei,
        // Already inside `effective_gas_price`, so counting it again would
        // double-charge the trade.
        priority_fee: 0,
        builder_payment: 0,
        sequencer_payment: 0,
        flash_fee: inputs.flash_fee_wei,
        dex_fees: inputs.dex_fees_wei,
        // Zero because the failure did not happen: this is the realized cost of
        // a transaction that succeeded, and the expected cost of failure is an
        // ex-ante quantity.
        expected_failure_cost: 0,
        calldata_bytes: inputs.calldata_bytes,
        compressed_data_estimate: inputs.compressed_data_estimate,
        gas_limit: inputs.gas_limit,
        gas_used_distribution: GasDistribution {
            p50: used,
            p90: used,
            p99: used,
            max_observed: used,
        },
    };

    // The balance delta of the profit token is already net of the DEX and flash
    // fees -- those moved tokens, and the delta is what is left. What it is NOT
    // net of is the chain's own fee, which is paid in the native token.
    let chain_fee = i128::try_from(receipt.total_chain_fee_wei()).unwrap_or(i128::MAX);
    let net = gross.saturating_sub(chain_fee);

    Ok(PnlAttribution {
        ticket_id: inputs.ticket,
        chain: inputs.chain,
        strategy: inputs.strategy,
        venues: inputs.venues.clone(),
        route_hash: inputs.route_hash,
        optimization_layers: inputs.optimization_layers.clone(),
        gross_profit: gross,
        realized_cost,
        net_profit_token: net,
        net_profit_usd_bounds: inputs.usd_bounds,
    })
}
