//! The Opportunity Ticket (Blueprint §2.5) -- the object the whole Capture
//! Assurance Protocol exists to account for.

use crate::commitment::ExecutionCommitment;
use crate::cost::GasLimit;
use crate::flash::FlashSourceQuote;
use crate::ids::{ChainId, SignerLaneId, StrategyId, SubmissionLaneId, TicketId};
use crate::pnl::PnlAttribution;
use crate::route::RouteCommitment;
use crate::sim::RevertClass;
use crate::state::{StateBranchId, StateFingerprint};
use crate::time::{DurationNanos, UnixNanos};
use alloy_primitives::{B256, U256};
use serde::{Deserialize, Serialize};

/// §2.5. MONOTONIC: [`TicketStatus::advance`] is the only mutator and refuses
/// anything that is not forward along this order.
///
/// The derived `Ord` IS the state machine -- variant order is load-bearing, so
/// do not reorder these to group them prettily.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TicketStatus {
    Observed,
    Reserved,
    Exacting,
    Simulated,
    Authorized,
    Signed,
    Dispatching,
    Acknowledged,
    Preconfirmed,
    Included,
    Finalized,
    Reconciled,
}

impl TicketStatus {
    /// Statuses at or past which a ticket holds reserved execution resources and
    /// must never be preempted (§57.1.2, INV-09).
    pub const fn is_authorized_or_later(self) -> bool {
        (self as u8) >= (Self::Authorized as u8)
    }

    /// Journal writes `fsync` from this point on (§17.5): before authorization a
    /// ticket carries no capital risk, so buffering is safe.
    pub const fn requires_durable_write(self) -> bool {
        self.is_authorized_or_later()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonotonicityError {
    pub from: TicketStatus,
    pub attempted: TicketStatus,
}

impl std::fmt::Display for MonotonicityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ticket status may only advance: {:?} -> {:?} is backwards or a no-op",
            self.from, self.attempted
        )
    }
}

impl std::error::Error for MonotonicityError {}

/// §2.5 terminal loss states. Every variant carries its cause -- §46.1 forbids
/// an unclassified outcome, and "it failed" without the why is unclassified in
/// every way that matters to the miss ledger.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TerminalFailure {
    Stale { observed_age: DurationNanos },
    StateChanged { expected: Box<StateFingerprint>, actual: Box<StateFingerprint> },
    EvCollapsed { admitted: i128, revalidated: i128 },
    RiskRejected { rule: String },
    DispatchTimeout { deadline: UnixNanos, elapsed: DurationNanos },
    NonceUnavailable { lane: SignerLaneId },
    SignerUnavailable,
    SubmissionRejected { lane: SubmissionLaneId, detail: String },
    CompetitorWon { observed_tx: Option<B256> },
    Reverted { revert_class: RevertClass, data: Vec<u8> },
    Diverged { branch: StateBranchId },
}

/// The closed world. §46.1: exactly one of these, always.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TicketOutcome {
    Success { stage: TicketStatus, realized: Box<PnlAttribution> },
    ExplicitFailure {
        code: TerminalFailure,
        at: UnixNanos,
        state: Box<StateFingerprint>,
        cause: String,
    },
}

impl TicketOutcome {
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }
}

/// When a ticket is allowed to land. Separate from the dispatch deadline: a
/// ticket can be dispatched on time and still be targeting a window that has
/// since closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionWindow {
    pub earliest: UnixNanos,
    pub latest: UnixNanos,
    /// Base only: the earliest Flashblock whose residual capacity fits this
    /// transaction's gas limit (§22.2).
    pub earliest_eligible_flashblock: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SubmissionPolicy {
    /// §24.4: the fallback, never the default.
    Public,
    Private,
    PrivateBundle,
}

/// Blueprint §2.5, all nineteen fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpportunityTicket {
    pub ticket_id: TicketId,
    pub chain_id: ChainId,
    pub strategy: StrategyId,
    pub state_fingerprint: StateFingerprint,
    pub route_commitment: RouteCommitment,
    pub exact_input: U256,
    pub expected_net_ev: i128,
    pub robustness_margin: f64,
    pub validity_start: UnixNanos,
    pub dispatch_deadline: UnixNanos,
    pub target_execution_window: ExecutionWindow,
    pub signer_lane: Option<SignerLaneId>,
    pub nonce: Option<u64>,
    pub flash_source: Option<FlashSourceQuote>,
    pub simulation_result_hash: B256,
    pub submission_policy: SubmissionPolicy,
    pub required_gas_limit: GasLimit,
    pub created_at: UnixNanos,
    pub status: TicketStatus,
}

impl OpportunityTicket {
    /// The only mutator of `status`. Refuses backwards moves and no-ops.
    pub fn advance(&mut self, to: TicketStatus) -> Result<(), MonotonicityError> {
        if to <= self.status {
            return Err(MonotonicityError { from: self.status, attempted: to });
        }
        self.status = to;
        Ok(())
    }

    /// §17.3: no ticket may remain queued past its dispatch deadline. The
    /// scheduler closes it with `DispatchTimeout` *before* the deadline passes,
    /// so expiry is always explained (INV-03).
    pub const fn is_past_deadline(&self, now: UnixNanos) -> bool {
        now.0 >= self.dispatch_deadline.0
    }

    pub const fn commitment_binds(&self, c: &ExecutionCommitment) -> bool {
        c.chain_id.0 == self.chain_id.0
    }
}
