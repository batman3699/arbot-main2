//! Task 8.5 R9 — the shadow plane's chain adapter (`apex_runtime::live::adapter`).
//!
//! It answers one question, from the latest capacity model, and refuses every
//! call that would reach a chain.

mod support;

use alloy_primitives::B256;
use apex_chain::adapter::{
    AdapterError, ChainExecutionAdapter, RejectReason, ReplacementPolicy, SignedPayload, SubmissionDecision,
};
use apex_chain::base::adapter::BaseAdapter;
use apex_chain::base::flashblock::{FlashblockObservation, MeasuredCapacityModel};
use apex_runtime::live::adapter::{LiveAdapter, RefusingRpc};
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, SubmissionLaneId};
use apex_types::time::UnixNanos;

const POLICY: ReplacementPolicy = ReplacementPolicy { supported: true, min_fee_bump_bps: 1_000, max_attempts: 2 };
const LANE: SubmissionLaneId = SubmissionLaneId(3);

fn adapter() -> LiveAdapter {
    LiveAdapter::new(vec![LANE, SubmissionLaneId(4)], POLICY)
}

/// Eight blocks' worth of one window with `budget` gas: enough to measure it.
fn model(budget: u64) -> MeasuredCapacityModel {
    let obs: Vec<_> =
        (0..8).map(|block| FlashblockObservation { block, index: 0, cumulative_gas_budget: budget }).collect();
    MeasuredCapacityModel::from_observations(&obs).unwrap()
}

/// The support candidate's p99 is 330,000 gas: 369,600 with §21.3's headroom.
const NEEDED: GasLimit = GasLimit(369_600);

/// **Before the first model, nothing fits** — rejected with no window, before
/// signing — and nothing is estimated as if it could land.
#[test]
fn before_the_first_model_every_submission_is_rejected() {
    let c = support::candidate(1, 100, 1_000_000);
    assert_eq!(BaseAdapter::<RefusingRpc>::safe_gas_limit(&c), NEEDED);
    let a = adapter();
    assert_eq!(
        a.optimize_submission_cost(&c, UnixNanos(0)),
        SubmissionDecision::Reject(RejectReason::NoSafeGasLimit { needed: NEEDED, largest_window: 0 })
    );
    assert!(a.estimate_total_fee(&c).is_err());
    assert_eq!(a.estimate_inclusion_probability(&c, UnixNanos(0)), 0.0);
}

/// **Each model rebuilds the adapter**: a window that fits admits the
/// candidate on the first lane; the next model, with none that fits, rejects
/// it quoting that model's largest window.
#[test]
fn each_model_rebuilds_the_adapter() {
    let c = support::candidate(1, 100, 1_000_000);
    let a = adapter();

    a.set_capacity(model(1_000_000));
    assert_eq!(
        a.optimize_submission_cost(&c, UnixNanos(0)),
        SubmissionDecision::Submit {
            lane: LANE,
            gas_limit: NEEDED,
            earliest_eligible_flashblock: Some(0),
            max_fee_per_gas_wei: c.total_execution_cost.l2_execution_fee,
            max_priority_fee_per_gas_wei: c.total_execution_cost.priority_fee,
        }
    );
    assert_eq!(a.estimate_total_fee(&c).unwrap().gas_limit, NEEDED);
    assert!(a.estimate_inclusion_probability(&c, UnixNanos(0)) > 0.0);

    a.set_capacity(model(100_000));
    assert_eq!(
        a.optimize_submission_cost(&c, UnixNanos(0)),
        SubmissionDecision::Reject(RejectReason::NoSafeGasLimit { needed: NEEDED, largest_window: 100_000 })
    );
}

fn refused<T: std::fmt::Debug>(r: Result<T, AdapterError>) {
    assert!(matches!(r, Err(AdapterError::NotSupportedOnThisChain { .. })), "{r:?}");
}

/// **Nothing reaches a chain**, with a model or without: every call a sending
/// lane would make is refused, and the regime — never probed — is not
/// admitted either.
#[tokio::test]
async fn nothing_reaches_a_chain() {
    let a = adapter();
    let payload = SignedPayload { chain: ChainId::BASE, hash: B256::ZERO, nonce: 0, gas_limit: NEEDED, raw: vec![] };
    for with_model in [false, true] {
        if with_model {
            a.set_capacity(model(1_000_000));
            assert!(matches!(a.regime(UnixNanos(0)), Err(AdapterError::Uninterpretable { .. })));
        } else {
            refused(a.regime(UnixNanos(0)));
        }
        refused(a.state_feed().await);
        refused(a.pending_state().await);
        refused(a.simulate(&payload).await);
        refused(a.submit(&payload, LANE).await);
        refused(a.observe_outcome(B256::ZERO).await);
        refused(a.reconcile_final_state(B256::ZERO).await);
    }
    assert_eq!(a.chain_id(), ChainId::BASE);
    assert_eq!(a.replacement_policy(), POLICY);
}
