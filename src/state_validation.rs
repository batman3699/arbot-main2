//! Background validation: does log-derived state match the chain?
//!
//! Runs entirely off the hot path (spec §4.5). The searcher only ever calls
//! `gate.trusted()`, a cheap read. A slow or failing RPC here downgrades trust
//! by letting the TTL expire; it never stalls anything.

use crate::live_state::LiveState;
use crate::metrics::Metrics;
use crate::reconcile::{compare_cl, compare_v2, Reconciliation};
use crate::state_gate::StateGate;
use crate::validation_select::{select, SelectOutcome};
use ethers::prelude::*;
use ethers::providers::JsonRpcClient;
use ethers::types::Address;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

/// Largest tick difference treated as intra-block noise rather than a decode
/// error.
///
/// A log describes state at a transaction; `eth_call` returns end-of-block
/// state, so a later swap in the same block drifts the tick slightly. Measured
/// live: every observed benign drift was 1-4 ticks, at 0-2 bps.
///
/// A DECODE error cannot land in this range. A sign-flipped `int24` gives a
/// delta near 400,000; a wrong word offset gives garbage. So a small threshold
/// separates the two cleanly, and requiring exactness — as an earlier version
/// did — failed 6 of 7 pools that were correct to within 2 bps.
pub(crate) const MAX_TICK_DRIFT: i32 = 16;

/// Turn one comparison into a gate verdict.
///
/// The two axes are independent: price divergence fails on bps, and a tick
/// error large enough to indicate a DECODE fault fails on its own regardless of
/// how good the price looks.
#[allow(dead_code)]
pub(crate) fn record_outcome(
    gate: &StateGate,
    pool: Address,
    bps: Option<i64>,
    tick_delta: Option<i32>,
) {
    let verdict = match (bps, tick_delta) {
        (None, _) => None,
        (Some(_), Some(t)) if t.abs() > MAX_TICK_DRIFT => Some(i64::MAX),
        (Some(b), _) => Some(b),
    };
    gate.record(pool, verdict);
}

#[allow(dead_code)]
fn emit(rec: &Reconciliation, pool: Address, block: u64, metrics: Option<&Arc<Metrics>>) {
    if let (Some(m), Some(bps)) = (metrics, rec.relative_delta_bps) {
        m.live_state_divergence_bps
            .with_label_values(&[rec.venue])
            .observe(bps as f64);
    }
    // The spec §9.1 reconciliation record, one line per check.
    debug!(
        target: "state_validation",
        pool = %format!("{pool:#x}"),
        venue = rec.venue,
        block,
        local = %rec.local,
        on_chain = %rec.on_chain,
        absolute_delta = %rec.absolute_delta,
        relative_delta_bps = ?rec.relative_delta_bps,
        tick_delta = ?rec.tick_delta,
        local_state_version = rec.local_state_version,
        anchor_id = rec.anchor_id,
        continuity_epoch = rec.continuity_epoch,
        trust_state = ?rec.trust_state,
        "state reconciliation"
    );
}

/// One validation pass. Returns the number of pools actually measured.
#[allow(dead_code)]
pub async fn validate_once<C>(
    provider: &Arc<Provider<C>>,
    live: &Arc<LiveState>,
    gate: &StateGate,
    metrics: Option<&Arc<Metrics>>,
) -> usize
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let Ok(head) = provider.get_block_number().await else {
        warn!("state validation: block number unavailable; skipping pass");
        return 0;
    };
    let head = head.as_u64();
    let mut measured = 0usize;

    let count = |outcome: &str| {
        if let Some(m) = metrics {
            m.live_state_checks.with_label_values(&[outcome]).inc();
        }
    };

    for pool in gate.due_for_check(live.tracked_v2()) {
        let Some(snap) = live.v2_snapshot(pool) else {
            continue;
        };
        let block = match select(snap.prov.ordinal, head) {
            SelectOutcome::Check { block } => block,
            SelectOutcome::NoOrdinal => {
                count("no_ordinal");
                continue;
            }
            SelectOutcome::TooOld { .. } => {
                count("skipped_lag");
                continue;
            }
        };
        // Pinned to the snapshot's block, NOT the head.
        let states = crate::quote_univ2::load_pair_states_batched(
            provider.clone(),
            &[pool],
            block.into(),
        )
        .await;
        match states.get(&pool) {
            Some(chain) => {
                let rec = compare_v2(&snap, chain);
                emit(&rec, pool, block, metrics);
                record_outcome(gate, pool, rec.relative_delta_bps, rec.tick_delta);
                count("measured");
                measured += 1;
            }
            None => {
                record_outcome(gate, pool, None, None);
                count("unreachable");
            }
        }
    }

    for pool in gate.due_for_check(live.tracked_cl()) {
        let Some(snap) = live.cl_snapshot(pool) else {
            continue;
        };
        let block = match select(snap.prov.ordinal, head) {
            SelectOutcome::Check { block } => block,
            SelectOutcome::NoOrdinal => {
                count("no_ordinal");
                continue;
            }
            SelectOutcome::TooOld { .. } => {
                count("skipped_lag");
                continue;
            }
        };
        // Zero token addresses skip the balanceOf sub-calls: this phase compares
        // slot0 and liquidity only, and balances are Phase 2b's concern.
        let req = [(pool, None, Address::zero(), Address::zero())];
        let states =
            crate::cl_sim::load_cl_pool_states_batched(provider.clone(), &req, block.into()).await;
        match states.get(&pool) {
            Some(chain) => {
                let rec = compare_cl(&snap, chain);
                emit(&rec, pool, block, metrics);
                record_outcome(gate, pool, rec.relative_delta_bps, rec.tick_delta);
                count("measured");
                measured += 1;
            }
            None => {
                record_outcome(gate, pool, None, None);
                count("unreachable");
            }
        }
    }

    if let Some(m) = metrics {
        let (trusted, untrusted, _) = gate.stats();
        m.live_state_trusted.set(trusted as f64);
        m.live_state_untrusted.set(untrusted as f64);
    }
    measured
}

/// Loop forever, validating a bounded sample each pass.
#[allow(dead_code)]
pub async fn run_state_validation<C>(
    provider: Arc<Provider<C>>,
    live: Arc<LiveState>,
    metrics: Option<Arc<Metrics>>,
    interval: Duration,
) where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let gate = crate::state_gate::gate();
    loop {
        let measured = validate_once(&provider, &live, gate, metrics.as_ref()).await;
        if measured > 0 {
            let (trusted, untrusted, total) = gate.stats();
            debug!(measured, trusted, untrusted, total, "state validation pass");
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed read must record NOTHING. Recording it as agreement would grant
    /// trust on the strength of an RPC outage — the exact failure the gate
    /// exists to prevent.
    #[test]
    fn an_unreachable_pool_records_no_verdict() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(1);
        record_outcome(&gate, pool, None, None);
        assert!(
            !gate.trusted(pool),
            "an unreachable read must not grant trust"
        );
    }

    #[test]
    fn a_matching_pool_records_a_passing_verdict() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(2);
        record_outcome(&gate, pool, Some(0), None);
        assert!(gate.trusted(pool));
    }

    #[test]
    fn a_diverging_pool_records_a_failing_verdict() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(3);
        record_outcome(&gate, pool, Some(4_000), None);
        assert!(!gate.trusted(pool));
    }

    /// Small tick drift is block-granularity noise, not decoder error: a later
    /// swap in the same block moves the tick a little, and `eth_call` returns
    /// end-of-block state. Measured live: 6 of 7 non-zero tick deltas sat
    /// within +/-2 bps with deltas of 1-4, and failing them cost real coverage.
    #[test]
    fn a_small_tick_drift_does_not_disqualify() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        for (i, (bps, tick)) in [(-1, -2), (0, -1), (2, 4), (1, 3)].iter().enumerate() {
            let pool = ethers::types::Address::from_low_u64_be(100 + i as u64);
            record_outcome(&gate, pool, Some(*bps), Some(*tick));
            assert!(
                gate.trusted(pool),
                "bps={bps} tick_delta={tick} is intra-block noise, not a decode error"
            );
        }
    }

    /// A sign-flipped tick is a decode error and must still fail, however good
    /// the price looks. That is what the threshold exists to separate.
    #[test]
    fn a_sign_flipped_tick_still_fails() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(200);
        record_outcome(&gate, pool, Some(0), Some(-396_476));
        assert!(!gate.trusted(pool));
    }

    #[test]
    fn a_tick_delta_just_over_the_threshold_fails() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(201);
        record_outcome(&gate, pool, Some(0), Some(MAX_TICK_DRIFT + 1));
        assert!(!gate.trusted(pool));
    }

    #[test]
    fn a_tick_delta_at_the_threshold_is_tolerated() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(202);
        record_outcome(&gate, pool, Some(0), Some(MAX_TICK_DRIFT));
        assert!(gate.trusted(pool));
    }

    /// Divergent price still fails even when the tick is fine -- the two axes
    /// are independent.
    #[test]
    fn a_large_bps_still_fails_with_a_tolerable_tick() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(203);
        record_outcome(&gate, pool, Some(163), Some(-4));
        assert!(!gate.trusted(pool), "163 bps is divergent regardless of tick");
    }

    #[test]
    fn a_zero_tick_delta_does_not_disqualify() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(5);
        record_outcome(&gate, pool, Some(1), Some(0));
        assert!(gate.trusted(pool));
    }
}
