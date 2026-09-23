//! `BaseAdapter` — all eleven `ChainExecutionAdapter` methods (§21).
//!
//! # The RPC is injected
//!
//! `BaseAdapter` is generic over [`BaseRpc`], the five things it actually needs
//! from a node. That is the same seam used throughout: the decisions here —
//! gas-limit minimization, flashblock eligibility, fee estimation, replacement
//! — are pure and tested offline, and the I/O is one small trait a test can
//! double. An adapter that owned a `Provider` would be an adapter nobody could
//! test without a network, and this repository's egress is already the reason
//! several Phase 2-4 measurements are outstanding.
//!
//! # §21.3's rejection happens here, before signing
//!
//! ```text
//! G_safe = Q_gas(simulated gas) + headroom
//! choose smallest gas_limit >= G_safe that remains valid AND fits the earliest
//!   economically valuable inclusion window
//! if none exists → REJECT THE TICKET BEFORE SIGNING
//! ```
//!
//! `optimize_submission_cost` is a sync method taking `&Candidate`, so it
//! cannot sign anything — the rejection is structurally before the signer
//! rather than merely earlier in a function.

use crate::adapter::{
    Ack, AdapterError, AdapterResult, ChainExecutionAdapter, PendingState, RejectReason,
    ReplacementPolicy, SignedPayload, StateFeedHandle, SubmissionDecision,
};
use crate::base::flashblock::{earliest_eligible_from, MeasuredCapacityModel};
use crate::regime::{ChainRegime, RegimeDiscovery};
use alloy_primitives::B256;
use apex_types::candidate::Candidate;
use apex_types::cost::{GasLimit, TotalExecutionCost};
use apex_types::ids::{ChainId, SubmissionLaneId};
use apex_types::miss::ObservedOutcome;
use apex_types::pnl::PnlAttribution;
use apex_types::sim::SimulationResult;
use apex_types::time::{DurationNanos, UnixNanos};
use async_trait::async_trait;

/// Base's preconfirmation round. 200 ms per flashblock, ten to a 2 s block.
pub const FLASHBLOCK: DurationNanos = DurationNanos(200_000_000);

/// The five things the adapter needs from a node. Everything else it computes.
#[async_trait]
pub trait BaseRpc: Send + Sync {
    async fn pending_state(&self) -> AdapterResult<PendingState>;
    async fn state_feed(&self) -> AdapterResult<StateFeedHandle>;
    async fn simulate(&self, payload: &SignedPayload) -> AdapterResult<SimulationResult>;
    async fn submit(&self, signed: &SignedPayload, lane: SubmissionLaneId) -> AdapterResult<Ack>;
    async fn observe(&self, h: B256) -> AdapterResult<ObservedOutcome>;
    async fn attribute(&self, h: B256) -> AdapterResult<PnlAttribution>;
}

/// §21.3's headroom over the simulated figure, in basis points.
///
/// A gas limit is not a cost on Base, it is a *scheduling* variable (INV-19):
/// too tight and the transaction reverts out of gas; too loose and it lands in
/// a later flashblock than it needed to. 12% is the band between the p99 of the
/// gas distribution and the point where an extra window becomes likely at
/// realistic capacities — it is a starting value to be re-derived from
/// measurement, and it is stated here rather than spread through the code.
pub const GAS_HEADROOM_BPS: u64 = 1_200;

pub struct BaseAdapter<R: BaseRpc> {
    rpc: R,
    regime: RegimeDiscovery,
    capacity: MeasuredCapacityModel,
    replacement: ReplacementPolicy,
    /// Lanes this adapter may submit through, most preferred first.
    lanes: Vec<SubmissionLaneId>,
    /// How long a discovered regime stays admissible (§20.1).
    regime_ttl: DurationNanos,
}

impl<R: BaseRpc> BaseAdapter<R> {
    pub fn new(
        rpc: R,
        regime: RegimeDiscovery,
        capacity: MeasuredCapacityModel,
        replacement: ReplacementPolicy,
        lanes: Vec<SubmissionLaneId>,
        regime_ttl: DurationNanos,
    ) -> Self {
        Self { rpc, regime, capacity, replacement, lanes, regime_ttl }
    }

    pub const fn capacity_model(&self) -> &MeasuredCapacityModel {
        &self.capacity
    }

    /// The injected RPC, so a test can assert what the adapter did **not** ask
    /// it to do. §21.3's "reject before signing" is a claim about an absence,
    /// and an absence can only be checked by counting.
    pub const fn rpc_for_test(&self) -> &R {
        &self.rpc
    }

    /// §21.3's `G_safe`: the p99 of the gas distribution plus headroom.
    ///
    /// p99, not p50: the limit must cover the tail, because a transaction that
    /// runs out of gas has paid for the whole thing and bought nothing.
    pub const fn safe_gas_limit(c: &Candidate) -> GasLimit {
        let p99 = c.total_execution_cost.gas_used_distribution.p99.0;
        GasLimit(p99 + (p99 * GAS_HEADROOM_BPS) / 10_000)
    }

    /// The flashblock index the candidate could land in, given where the block
    /// already is. INV-38: never behind `current_index`.
    pub fn eligible_window(&self, c: &Candidate, current_index: u32) -> Option<u32> {
        earliest_eligible_from(current_index, Self::safe_gas_limit(c).0, &self.capacity)
    }
}

#[async_trait]
impl<R: BaseRpc> ChainExecutionAdapter for BaseAdapter<R> {
    fn chain_id(&self) -> ChainId {
        ChainId::BASE
    }

    fn regime(&self, now: UnixNanos) -> AdapterResult<ChainRegime> {
        self.regime
            .admit_to_live_trading(now, self.regime_ttl)
            .copied()
            .map_err(|e| AdapterError::Uninterpretable { detail: e.to_string() })
    }

    async fn state_feed(&self) -> AdapterResult<StateFeedHandle> {
        self.rpc.state_feed().await
    }

    async fn pending_state(&self) -> AdapterResult<PendingState> {
        self.rpc.pending_state().await
    }

    async fn simulate(&self, payload: &SignedPayload) -> AdapterResult<SimulationResult> {
        self.rpc.simulate(payload).await
    }

    fn estimate_total_fee(&self, c: &Candidate) -> AdapterResult<TotalExecutionCost> {
        // The candidate already carries a cost built by `apex-econ`, which owns
        // the Fjord L1 fee formula and the failure model. What this adds is the
        // chain's own correction: the gas LIMIT the scheduler will actually
        // use, which differs from the simulated figure by the headroom above.
        let mut cost = c.total_execution_cost.clone();
        cost.gas_limit = Self::safe_gas_limit(c);
        Ok(cost)
    }

    fn estimate_inclusion_probability(&self, c: &Candidate, at: UnixNanos) -> f64 {
        // Decays with how much of the candidate's validity window is left. A
        // candidate whose state has already expired cannot land at all, and
        // reporting anything above zero for it is how a dead opportunity gets
        // signed.
        //
        // There are deliberately no early-exit guards for "past the deadline" or
        // "no windows left". Both were written and both turned out to be dead:
        // the saturating subtraction yields zero windows past the deadline, and
        // at zero windows `1 - 1/(1+0)` is already exactly 0.0. Mutation testing
        // found them, and code that reads as load-bearing but is not is worse
        // than no code at all -- it is the first thing a future reader would
        // trust and the last thing they would test.
        let remaining = c.deadline.0.saturating_sub(at.0);
        let windows_left = remaining / FLASHBLOCK.0.max(1);
        // Capped by the candidate's own capture probability: the chain cannot
        // make a trade more likely than the strategy that found it believes.
        let by_time = 1.0 - (1.0 / (1.0 + windows_left as f64));
        by_time.min(c.capture_probability).clamp(0.0, 1.0)
    }

    fn optimize_submission_cost(&self, c: &Candidate, now: UnixNanos) -> SubmissionDecision {
        let Some(&lane) = self.lanes.first() else {
            return SubmissionDecision::Reject(RejectReason::NoLaneForPolicy);
        };
        let needed = Self::safe_gas_limit(c);

        // §22.2, from index 0: this is a pre-signing decision about a candidate,
        // not a re-decision mid-block, so there is no current index to be
        // behind. `eligible_window` is what takes one.
        let Some(k) = earliest_eligible_from(0, needed.0, &self.capacity) else {
            return SubmissionDecision::Reject(RejectReason::NoSafeGasLimit {
                needed,
                largest_window: self.capacity.largest_measured_window(),
            });
        };

        // §21.2: "state validity at that time". A window the state will not
        // survive to is not an eligible window -- and the scheduler must say so
        // here rather than let last-mile revalidation discover it, because by
        // then a signature has been spent.
        let earliest_landing = DurationNanos(FLASHBLOCK.0.saturating_mul(u64::from(k)));
        let valid_for = DurationNanos(c.deadline.0.saturating_sub(now.0));
        if valid_for.0 < earliest_landing.0 {
            return SubmissionDecision::Reject(RejectReason::StateExpiresFirst {
                valid_for,
                earliest_landing,
            });
        }

        // What this adapter ADDED to the cost, not a re-check of the candidate's
        // own arithmetic. `expected_net_profit` is already net of
        // `total_execution_cost`; what nothing upstream knew is that §21.3
        // raised the gas limit to cover the tail, and that extra gas is paid
        // for at the same price per unit.
        let priced_limit = c.total_execution_cost.gas_limit.0.max(1);
        let extra_gas = u128::from(needed.0.saturating_sub(priced_limit));
        let extra_wei =
            extra_gas.saturating_mul(c.total_execution_cost.l2_execution_fee) / u128::from(priced_limit);
        let edge_wei = u128::try_from(c.expected_net_profit.max(0)).unwrap_or(0);
        if extra_wei > edge_wei {
            return SubmissionDecision::Reject(RejectReason::GasHeadroomExceedsEdge {
                extra_wei,
                edge_wei,
            });
        }

        SubmissionDecision::Submit {
            lane,
            gas_limit: needed,
            earliest_eligible_flashblock: Some(k),
            max_fee_per_gas_wei: c.total_execution_cost.l2_execution_fee,
            max_priority_fee_per_gas_wei: c.total_execution_cost.priority_fee,
        }
    }

    async fn submit(&self, signed: &SignedPayload, lane: SubmissionLaneId) -> AdapterResult<Ack> {
        self.rpc.submit(signed, lane).await
    }

    fn replacement_policy(&self) -> ReplacementPolicy {
        self.replacement
    }

    async fn observe_outcome(&self, h: B256) -> AdapterResult<ObservedOutcome> {
        self.rpc.observe(h).await
    }

    async fn reconcile_final_state(&self, h: B256) -> AdapterResult<PnlAttribution> {
        self.rpc.attribute(h).await
    }
}
