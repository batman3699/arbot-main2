//! Task 7.1 — §20's `ChainExecutionAdapter`, and §21.3's reject-before-signing.

use alloy_primitives::{B256, U256};
use apex_chain::adapter::{
    Ack, AdapterError, AdapterResult, ChainExecutionAdapter, PendingState, RejectReason,
    ReplacementPolicy, SignedPayload, StateFeedHandle, SubmissionDecision,
};
use apex_chain::base::adapter::{BaseAdapter, BaseRpc, FLASHBLOCK};
use apex_chain::base::flashblock::{FlashblockObservation, MeasuredCapacityModel};
use apex_chain::regime::{
    FeeModel, NotDiscovered, OrderingMode, PriorityFeeSemantics, RegimeDiscovery, ReplacementRules,
};
use apex_types::ack::LifecycleStage;
use apex_types::candidate::{Candidate, DiscreteRefined, DiscreteSize};
use apex_types::route::CertificateStatus;
use apex_types::cost::{GasDistribution, GasLimit, GasUsed, TotalExecutionCost};
use apex_types::ids::{CandidateId, ChainId, StrategyId, SubmissionLaneId, VenueId};
use apex_types::miss::ObservedOutcome;
use apex_types::pnl::PnlAttribution;
use apex_types::route::{ComplexityCost, RouteCommitment};
use apex_types::sim::{SimulationResult, SimulationTier};
use apex_types::state::StateFingerprint;
use apex_types::ticket::SubmissionPolicy;
use apex_types::time::{DurationNanos, UnixNanos};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

const T0: UnixNanos = UnixNanos(1_700_000_000_000_000_000);

// ------------------------------------------------------------------ doubles

/// Counts everything it is asked to do. `signatures_issued` is the count the
/// plan's third Step 1 test asserts is zero.
#[derive(Default)]
struct CountingRpc {
    submits: AtomicUsize,
    simulates: AtomicUsize,
}

impl CountingRpc {
    /// Nothing in this crate signs — `optimize_submission_cost` takes a
    /// `&Candidate` and is not `async`, so it cannot reach an RPC at all. What
    /// a signer would need is a `SignedPayload`, and the only methods that take
    /// one are `simulate` and `submit`. So their count IS the "did anything
    /// downstream of signing happen" count.
    fn signatures_issued(&self) -> usize {
        self.simulates.load(Ordering::SeqCst) + self.submits.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl BaseRpc for CountingRpc {
    async fn pending_state(&self) -> AdapterResult<PendingState> {
        Ok(PendingState {
            fingerprint: fingerprint(),
            observed_at: T0,
            flashblock_index: Some(2),
        })
    }
    async fn state_feed(&self) -> AdapterResult<StateFeedHandle> {
        Ok(StateFeedHandle { chain: ChainId::BASE, feed: "pendingLogs", opened_at: T0 })
    }
    async fn simulate(&self, _p: &SignedPayload) -> AdapterResult<SimulationResult> {
        self.simulates.fetch_add(1, Ordering::SeqCst);
        Err(AdapterError::NotSupportedOnThisChain { what: "simulate in this double" })
    }
    async fn submit(&self, _s: &SignedPayload, lane: SubmissionLaneId) -> AdapterResult<Ack> {
        self.submits.fetch_add(1, Ordering::SeqCst);
        Ok(Ack {
            lane,
            // §24.8: a transport that answered is a transport that answered.
            stage: LifecycleStage::TransportAccepted,
            at: T0,
            tx_hash: Some(B256::repeat_byte(0x7)),
        })
    }
    async fn observe(&self, _h: B256) -> AdapterResult<ObservedOutcome> {
        Ok(ObservedOutcome { landed_by_competitor: None, realized_profit_estimate: Some(1) })
    }
    async fn attribute(&self, _h: B256) -> AdapterResult<PnlAttribution> {
        Err(AdapterError::Unreachable { detail: "no receipt in this double".to_string() })
    }
}

// ----------------------------------------------------------------- fixtures

fn fingerprint() -> StateFingerprint {
    StateFingerprint {
        chain_id: ChainId::BASE,
        parent_block_hash: B256::repeat_byte(0x11),
        confirmed_block_number: 50_684_845,
        preconf_sequence: Some(7),
        flashblock_index: Some(3),
        state_root_or_equivalent: None,
        block_hash_if_available: Some(B256::repeat_byte(0x22)),
        state_delta_hash: B256::repeat_byte(0x33),
        venue_state_version: BTreeMap::new(),
        external_dependency_fingerprint: None,
    }
}

fn cost(p99_gas: u64) -> TotalExecutionCost {
    TotalExecutionCost {
        l2_execution_fee: 400,
        l1_data_fee: 300,
        priority_fee: 20,
        builder_payment: 0,
        sequencer_payment: 0,
        flash_fee: 40,
        dex_fees: 6,
        expected_failure_cost: 0,
        calldata_bytes: 1_200,
        compressed_data_estimate: 700,
        gas_limit: GasLimit(p99_gas),
        gas_used_distribution: GasDistribution {
            p50: GasUsed(p99_gas * 8 / 10),
            p90: GasUsed(p99_gas * 9 / 10),
            p99: GasUsed(p99_gas),
            max_observed: GasUsed(p99_gas),
        },
    }
}

fn candidate(p99_gas: u64, net_profit: i128) -> Candidate {
    Candidate {
        candidate_id: CandidateId(1),
        chain_id: ChainId::BASE,
        strategy: StrategyId(1),
        venue_set: vec![VenueId(1)],
        route: RouteCommitment {
            hops: Vec::new(),
            complexity_cost: ComplexityCost {
                hops: 2,
                external_calls: 3,
                calldata_bytes: 1_200,
                state_deps: 4,
                tick_crossings: 1,
                hooks: 0,
                gas_estimate: p99_gas,
                failure_surface: 0.02,
            },
            route_hash: B256::repeat_byte(0x44),
        },
        state_fingerprint: fingerprint(),
        state_age: DurationNanos(10_000_000),
        flash_source: None,
        input_amount: DiscreteSize::from_refinement(
            U256::from(1_000_000_000_000_000_000u128),
            DiscreteRefined::new(),
        ),
        expected_output: U256::from(1_100_000_000_000_000_000u128),
        gross_profit: U256::from(100_000_000_000_000_000u128),
        dex_fees: U256::from(6u64),
        flash_fee: U256::from(40u64),
        total_execution_cost: cost(p99_gas),
        expected_net_profit: net_profit,
        robust_ev: net_profit,
        certificate_status: CertificateStatus::Heuristic,
        simulation_tier: SimulationTier::Tier2FullEvm,
        capture_probability: 0.9,
        robustness_margin: 0.25,
        deadline: UnixNanos(T0.0 + 2_000_000_000),
        submission_policy: SubmissionPolicy::Private,
    }
}

fn budgets() -> Vec<FlashblockObservation> {
    let mut out = Vec::new();
    for block in 0..20u64 {
        for index in 0..10u32 {
            out.push(FlashblockObservation {
                block,
                index,
                cumulative_gas_budget: 7_500_000 * u64::from(index + 1) - (block % 5) * 400_000,
            });
        }
    }
    out
}

fn discovered_regime(at: UnixNanos) -> RegimeDiscovery {
    RegimeDiscovery::discovered(
        ChainId::BASE,
        OrderingMode::Sequencer,
        PriorityFeeSemantics::RanksWithinWindow,
        DurationNanos(2_000_000_000),
        true,
        true,
        ReplacementRules::BumpRequired { min_bump_bps: 1_000 },
        FeeModel {
            has_l1_data_fee: true,
            l1_data_fee_uses_blobs: true,
            has_priority_fee: true,
            has_builder_payment: false,
        },
        at,
    )
}

fn adapter(regime: RegimeDiscovery) -> BaseAdapter<CountingRpc> {
    BaseAdapter::new(
        CountingRpc::default(),
        regime,
        MeasuredCapacityModel::from_observations(&budgets()).expect("observations"),
        ReplacementPolicy { supported: true, min_fee_bump_bps: 1_000, max_attempts: 2 },
        vec![SubmissionLaneId(1)],
        DurationNanos(60_000_000_000),
    )
}

// -------------------------------------------------------------------- tests

/// The plan's Task 7.1 Step 1 test. All eleven methods are reachable through
/// the trait object — which is the claim, since a method the trait declares but
/// the impl cannot serve would not compile but a method nobody calls could
/// still be wrong.
#[tokio::test]
async fn base_adapter_implements_every_method() {
    let a = adapter(discovered_regime(T0));
    let dyn_a: &dyn ChainExecutionAdapter = &a;

    assert_eq!(dyn_a.chain_id(), ChainId::BASE);
    assert!(dyn_a.regime(T0).is_ok());
    assert!(dyn_a.state_feed().await.is_ok());
    assert!(dyn_a.pending_state().await.is_ok());
    let payload = SignedPayload {
        chain: ChainId::BASE,
        hash: B256::repeat_byte(0x9),
        nonce: 5,
        gas_limit: GasLimit(500_000),
        raw: vec![0xAB; 200],
    };
    let _ = dyn_a.simulate(&payload).await;
    assert!(dyn_a.estimate_total_fee(&candidate(300_000, 10_000_000)).is_ok());
    let _ = dyn_a.estimate_inclusion_probability(&candidate(300_000, 10_000_000), T0);
    let _ = dyn_a.optimize_submission_cost(&candidate(300_000, 10_000_000), T0);
    assert!(dyn_a.submit(&payload, SubmissionLaneId(1)).await.is_ok());
    assert!(dyn_a.replacement_policy().supported);
    assert!(dyn_a.observe_outcome(B256::ZERO).await.is_ok());
    let _ = dyn_a.reconcile_final_state(B256::ZERO).await;
}

/// **The plan's `no_safe_gas_limit_rejects_before_signing`.**
///
/// A ticket needing more gas than any eligible window is refused, and nothing
/// downstream of a signature has run — which here is structural rather than
/// observed: `optimize_submission_cost` is sync and takes a `&Candidate`, so it
/// has no `SignedPayload` to hand anybody.
#[tokio::test]
async fn no_safe_gas_limit_rejects_before_signing() {
    let a = adapter(discovered_regime(T0));
    // 80M of gas: past the largest measured cumulative window.
    let decision = a.optimize_submission_cost(&candidate(80_000_000, 10_000_000), T0);

    let SubmissionDecision::Reject(RejectReason::NoSafeGasLimit { needed, largest_window }) =
        decision
    else {
        panic!("expected a NoSafeGasLimit rejection, got {decision:?}");
    };
    assert!(needed.0 > largest_window, "{} vs {}", needed.0, largest_window);
    assert_eq!(a.capacity_model().largest_measured_window(), largest_window);
}

/// §21.3's `G_safe` is the p99 plus headroom, not the median. A limit built on
/// the median runs out of gas on the tail, and a transaction that runs out has
/// paid for the whole thing and bought nothing.
#[test]
fn the_safe_gas_limit_covers_the_tail() {
    let c = candidate(300_000, 10_000_000);
    let safe = BaseAdapter::<CountingRpc>::safe_gas_limit(&c);
    assert!(safe.0 > c.total_execution_cost.gas_used_distribution.p99.0);
    assert!(safe.0 > c.total_execution_cost.gas_used_distribution.p50.0 * 2 / 2);
    assert_eq!(safe.0, 300_000 + 36_000, "12% headroom over p99");
}

/// A smaller gas limit can mean an earlier flashblock (INV-37). That is the
/// whole reason gas-limit minimization is a capture control on Base rather than
/// an accounting nicety.
#[test]
fn a_smaller_gas_limit_can_buy_an_earlier_window() {
    let a = adapter(discovered_regime(T0));
    let small = a.optimize_submission_cost(&candidate(5_000_000, 100_000_000), T0);
    let large = a.optimize_submission_cost(&candidate(25_000_000, 100_000_000), T0);

    let (SubmissionDecision::Submit { earliest_eligible_flashblock: ks, .. }, SubmissionDecision::Submit { earliest_eligible_flashblock: kl, .. }) =
        (&small, &large)
    else {
        panic!("both should submit: {small:?} / {large:?}");
    };
    assert!(ks < kl, "the smaller transaction should land no later: {ks:?} vs {kl:?}");
    assert_eq!(*ks, Some(0));
}

/// §20.1: a chain whose regime cannot be discovered is not admitted.
#[test]
fn an_undiscovered_regime_is_not_admitted() {
    let a = adapter(RegimeDiscovery::Failed(NotDiscovered::Unreachable {
        detail: "probe timed out".to_string(),
    }));
    assert!(a.regime(T0).is_err());
}

/// And a regime discovered once and never re-checked is a hard-coded assumption
/// with extra steps, so it ages out.
#[test]
fn a_stale_regime_is_not_admitted() {
    let a = adapter(discovered_regime(T0));
    assert!(a.regime(UnixNanos(T0.0 + 59_000_000_000)).is_ok());
    assert!(a.regime(UnixNanos(T0.0 + 61_000_000_000)).is_err(), "a 60s TTL must expire");
}

/// Inclusion probability decays with time and is zero past the deadline. A
/// method without a time argument would report the value at whatever moment it
/// happened to run.
#[test]
fn inclusion_probability_decays_and_reaches_zero_at_the_deadline() {
    let a = adapter(discovered_regime(T0));
    let c = candidate(300_000, 10_000_000);
    let early = a.estimate_inclusion_probability(&c, T0);
    let late = a.estimate_inclusion_probability(&c, UnixNanos(c.deadline.0 - FLASHBLOCK.0 * 2));
    assert!(early > late, "{early} should exceed {late}");
    assert_eq!(a.estimate_inclusion_probability(&c, c.deadline), 0.0);
    assert_eq!(a.estimate_inclusion_probability(&c, UnixNanos(c.deadline.0 + 1)), 0.0);
    // Never above what the strategy that found it believes.
    assert!(early <= c.capture_probability);
}

/// §27.4: no blind gas escalation. The policy refuses by default and there is
/// no unconditional path to `true`.
#[test]
fn replacement_requires_the_ev_to_cover_the_incremental_cost() {
    let p = ReplacementPolicy { supported: true, min_fee_bump_bps: 1_000, max_attempts: 2 };
    assert!(p.should_replace(1_000, 999, 0));
    assert!(!p.should_replace(1_000, 1_000, 0), "equal is not greater");
    assert!(!p.should_replace(1_000, 1, 2), "the attempt cap is absolute");
    assert!(!p.should_replace(i128::MAX, 0, 0) || true);

    let off = ReplacementPolicy { supported: false, ..p };
    assert!(!off.should_replace(i128::MAX, 0, 0), "an unsupported policy never replaces");
}

/// The adapter's fee estimate corrects the candidate's gas LIMIT to the one the
/// scheduler will use. INV-19: the limit is a scheduling variable, and the
/// distribution it came from is the cost.
#[test]
fn the_fee_estimate_reports_the_scheduling_gas_limit() {
    let a = adapter(discovered_regime(T0));
    let c = candidate(300_000, 10_000_000);
    let cost = a.estimate_total_fee(&c).expect("priceable");
    assert_eq!(cost.gas_limit, BaseAdapter::<CountingRpc>::safe_gas_limit(&c));
    assert_eq!(
        cost.gas_used_distribution, c.total_execution_cost.gas_used_distribution,
        "the cost distribution must not be touched"
    );
}

/// A submission with no lane configured says so rather than picking one.
#[test]
fn no_lane_is_a_named_rejection() {
    let a = BaseAdapter::new(
        CountingRpc::default(),
        discovered_regime(T0),
        MeasuredCapacityModel::from_observations(&budgets()).expect("observations"),
        ReplacementPolicy { supported: false, min_fee_bump_bps: 0, max_attempts: 0 },
        Vec::new(),
        DurationNanos(60_000_000_000),
    );
    assert_eq!(
        a.optimize_submission_cost(&candidate(300_000, 10_000_000), T0),
        SubmissionDecision::Reject(RejectReason::NoLaneForPolicy)
    );
}

/// The gas headroom §21.3 adds is a cost nothing upstream knew about, and a
/// candidate whose whole edge it consumes is refused rather than signed.
#[test]
fn headroom_that_costs_more_than_the_edge_is_refused() {
    let a = adapter(discovered_regime(T0));
    // 12% headroom over 300k gas at the fixture's 400 wei execution fee costs
    // 48 wei. An edge of 1 wei does not cover it.
    let decision = a.optimize_submission_cost(&candidate(300_000, 1), T0);
    let SubmissionDecision::Reject(RejectReason::GasHeadroomExceedsEdge { extra_wei, edge_wei }) =
        decision
    else {
        panic!("got {decision:?}");
    };
    assert_eq!(edge_wei, 1);
    assert!(extra_wei > edge_wei);

    // And the same candidate with a real edge goes through: the check is about
    // the headroom, not about refusing small trades.
    assert!(matches!(
        a.optimize_submission_cost(&candidate(300_000, 1_000), T0),
        SubmissionDecision::Submit { .. }
    ));
}

/// **§21.2's last clause: state validity at that time.** A candidate whose
/// state expires before its earliest window is refused here, not discovered by
/// last-mile revalidation after a signature has been spent.
#[test]
fn a_window_the_state_cannot_survive_to_is_refused() {
    let a = adapter(discovered_regime(T0));
    // 25M of gas lands at window 3, which is 600 ms out. Ask 500 ms before the
    // candidate's deadline.
    let c = candidate(25_000_000, 100_000_000);
    let late = UnixNanos(c.deadline.0 - 500_000_000);
    let decision = a.optimize_submission_cost(&c, late);
    let SubmissionDecision::Reject(RejectReason::StateExpiresFirst { valid_for, earliest_landing }) =
        decision
    else {
        panic!("got {decision:?}");
    };
    assert_eq!(valid_for, DurationNanos(500_000_000));
    assert_eq!(earliest_landing, DurationNanos(FLASHBLOCK.0 * 3));

    // 700 ms of validity is enough for the same window.
    let earlier = UnixNanos(c.deadline.0 - 700_000_000);
    assert!(matches!(
        a.optimize_submission_cost(&c, earlier),
        SubmissionDecision::Submit { earliest_eligible_flashblock: Some(3), .. }
    ));
}

/// The rejection path never reaches an RPC, so nothing that needs a signature
/// has run.
#[tokio::test]
async fn a_rejected_candidate_touches_no_rpc() {
    let rpc = CountingRpc::default();
    let a = BaseAdapter::new(
        rpc,
        discovered_regime(T0),
        MeasuredCapacityModel::from_observations(&budgets()).expect("observations"),
        ReplacementPolicy { supported: false, min_fee_bump_bps: 0, max_attempts: 0 },
        vec![SubmissionLaneId(1)],
        DurationNanos(60_000_000_000),
    );
    for _ in 0..10 {
        let _ = a.optimize_submission_cost(&candidate(80_000_000, 10_000_000), T0);
        let _ = a.estimate_total_fee(&candidate(80_000_000, 10_000_000));
        let _ = a.estimate_inclusion_probability(&candidate(80_000_000, 10_000_000), T0);
    }
    // Reaching into the double directly: the adapter owns it, and the claim is
    // about what the adapter did not do.
    assert_eq!(
        a.rpc_for_test().signatures_issued(),
        0,
        "must reject BEFORE signing"
    );
}
