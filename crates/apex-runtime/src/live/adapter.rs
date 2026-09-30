//! The chain adapter the shadow plane holds (Task 8.5 R9).
//!
//! # One question, and no chain to ask
//!
//! The plane consults its adapter for one thing: `optimize_submission_cost`,
//! which lane and what gas limit — §22.2's earliest eligible flashblock, from
//! the measured capacity model. That reads no chain. Everything that would
//! reach one — submit, simulate, observe, attribute — belongs to a lane that
//! sends, and the shadow plane's lane is the null dispatcher; so every such
//! call goes to [`RefusingRpc`], and is told no.
//!
//! # Rebuilt with each capacity model
//!
//! `BaseAdapter` takes its model at construction, and R5's sampler replaces the
//! model through a 14-day run. [`LiveAdapter`] holds the adapter behind a
//! version and rebuilds it on each new model; until the first, every
//! submission is rejected as having no measured window — the gas check failing
//! closed, as the live reader's does.
//!
//! The regime is recorded as **not discovered**: no probe has run, because a
//! shadow run submits nothing and nothing it does asks the regime. A regime
//! stated without a probe would be the hard-coded assumption §20.1 exists to
//! prevent.

use alloy_primitives::B256;
use apex_chain::adapter::{
    Ack, AdapterError, AdapterResult, ChainExecutionAdapter, PendingState, RejectReason, ReplacementPolicy,
    SignedPayload, StateFeedHandle, SubmissionDecision,
};
use apex_chain::base::adapter::{BaseAdapter, BaseRpc};
use apex_chain::base::flashblock::MeasuredCapacityModel;
use apex_chain::regime::{ChainRegime, NotDiscovered, RegimeDiscovery};
use apex_state::Versioned;
use apex_types::candidate::Candidate;
use apex_types::cost::TotalExecutionCost;
use apex_types::ids::{ChainId, SubmissionLaneId};
use apex_types::miss::ObservedOutcome;
use apex_types::pnl::PnlAttribution;
use apex_types::sim::SimulationResult;
use apex_types::state::ReconstructionStatus;
use apex_types::time::{DurationNanos, UnixNanos};

fn refused<T>() -> AdapterResult<T> {
    Err(AdapterError::NotSupportedOnThisChain { what: "a shadow run's chain adapter" })
}

/// A `BaseRpc` that refuses every call: see the module docs.
#[derive(Debug, Default)]
pub struct RefusingRpc;

#[async_trait::async_trait]
impl BaseRpc for RefusingRpc {
    async fn pending_state(&self) -> AdapterResult<PendingState> {
        refused()
    }
    async fn state_feed(&self) -> AdapterResult<StateFeedHandle> {
        refused()
    }
    async fn simulate(&self, _: &SignedPayload) -> AdapterResult<SimulationResult> {
        refused()
    }
    async fn submit(&self, _: &SignedPayload, _: SubmissionLaneId) -> AdapterResult<Ack> {
        refused()
    }
    async fn observe(&self, _: B256) -> AdapterResult<ObservedOutcome> {
        refused()
    }
    async fn attribute(&self, _: B256) -> AdapterResult<PnlAttribution> {
        refused()
    }
}

/// `BaseAdapter` over the latest measured capacity model.
pub struct LiveAdapter {
    lanes: Vec<SubmissionLaneId>,
    replacement: ReplacementPolicy,
    inner: Versioned<Option<BaseAdapter<RefusingRpc>>>,
}

impl LiveAdapter {
    pub fn new(lanes: Vec<SubmissionLaneId>, replacement: ReplacementPolicy) -> Self {
        Self { lanes, replacement, inner: Versioned::new(None, ReconstructionStatus::Rebuilding) }
    }

    /// Rebuild the adapter over a new model.
    pub fn set_capacity(&self, model: MeasuredCapacityModel) {
        let regime = RegimeDiscovery::Failed(NotDiscovered::Unreachable {
            detail: "not probed: a shadow run submits nothing and asks no regime".into(),
        });
        // No TTL: a regime never discovered has no age to bound.
        let adapter =
            BaseAdapter::new(RefusingRpc, regime, model, self.replacement, self.lanes.clone(), DurationNanos(0));
        self.inner.store(Some(adapter), ReconstructionStatus::Verified);
    }
}

#[async_trait::async_trait]
impl ChainExecutionAdapter for LiveAdapter {
    fn chain_id(&self) -> ChainId {
        ChainId::BASE
    }

    fn regime(&self, now: UnixNanos) -> AdapterResult<ChainRegime> {
        match &*self.inner.load().value {
            Some(a) => a.regime(now),
            None => refused(),
        }
    }

    async fn state_feed(&self) -> AdapterResult<StateFeedHandle> {
        RefusingRpc.state_feed().await
    }

    async fn pending_state(&self) -> AdapterResult<PendingState> {
        RefusingRpc.pending_state().await
    }

    async fn simulate(&self, p: &SignedPayload) -> AdapterResult<SimulationResult> {
        RefusingRpc.simulate(p).await
    }

    fn estimate_total_fee(&self, c: &Candidate) -> AdapterResult<TotalExecutionCost> {
        match &*self.inner.load().value {
            Some(a) => a.estimate_total_fee(c),
            None => refused(),
        }
    }

    fn estimate_inclusion_probability(&self, c: &Candidate, at: UnixNanos) -> f64 {
        match &*self.inner.load().value {
            Some(a) => a.estimate_inclusion_probability(c, at),
            None => 0.0,
        }
    }

    /// Before the first model, no window is measured: rejected before signing,
    /// as §21.3 requires of a gas limit that fits nothing known.
    fn optimize_submission_cost(&self, c: &Candidate, now: UnixNanos) -> SubmissionDecision {
        match &*self.inner.load().value {
            Some(a) => a.optimize_submission_cost(c, now),
            None => SubmissionDecision::Reject(RejectReason::NoSafeGasLimit {
                needed: BaseAdapter::<RefusingRpc>::safe_gas_limit(c),
                largest_window: 0,
            }),
        }
    }

    async fn submit(&self, p: &SignedPayload, lane: SubmissionLaneId) -> AdapterResult<Ack> {
        RefusingRpc.submit(p, lane).await
    }

    fn replacement_policy(&self) -> ReplacementPolicy {
        self.replacement
    }

    async fn observe_outcome(&self, h: B256) -> AdapterResult<ObservedOutcome> {
        RefusingRpc.observe(h).await
    }

    async fn reconcile_final_state(&self, h: B256) -> AdapterResult<PnlAttribution> {
        RefusingRpc.attribute(h).await
    }
}
