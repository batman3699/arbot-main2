//! Test doubles for the ports the control plane is written against.
//!
//! Every one of these is a *port implementation*, not a mock of the plane. The
//! plane under test is the real one: the real `TicketRegistry`, the real
//! journal, the real `SignerPool`, the real `last_mile` revalidation, the real
//! `DispatchGate`. What is faked is only what sits outside the process — a
//! search, a simulator, a signer, a chain — and two of those (`apex-search`,
//! `apex-exec`) are crates later phases build.
//!
//! That split is the deliverable. The defect recorded against the 16,659-line
//! `main.rs` was not that it was long; it was "working, untestable as
//! components". A plane whose whole lifecycle runs with no socket open is the
//! observable form of that defect being gone.

#![allow(dead_code)]

use alloy_primitives::{Address, B256, U256};
use apex_capture::recover::ChainOutcomeSource;
use apex_chain::adapter::{
    Ack, AdapterResult, ChainExecutionAdapter, PendingState, ReplacementPolicy, SignedPayload,
    StateFeedHandle, SubmissionDecision,
};
use apex_chain::regime::ChainRegime;
use apex_runtime::bus::{EventKind, StateEvent};
use apex_runtime::plane::{
    CandidateSource, Commitments, Decline, Economics, LiveReader, LiveReadings, RiskGate,
    SettlementFeed, Signer, Simulator,
};
use apex_types::ack::LifecycleStage;
use apex_types::candidate::{Candidate, DiscreteRefined, DiscreteSize};
use apex_types::cost::{GasDistribution, GasLimit, GasUsed, TotalExecutionCost};
use apex_types::ids::{
    CandidateId, ChainId, PoolId, StrategyId, SubmissionLaneId, TokenId, VenueId,
};
use apex_types::miss::ObservedOutcome;
use apex_types::pnl::{OptimizationLayer, PnlAttribution, UsdBounds};
use apex_types::route::{CertificateStatus, ComplexityCost, RouteCommitment, RouteHop};
use apex_types::sim::{SimulationResult, SimulationTier};
use apex_types::state::StateFingerprint;
use apex_types::ticket::{
    OpportunityTicket, SubmissionPolicy, TerminalFailure, TicketOutcome, TicketStatus,
};
use apex_types::time::{DurationNanos, UnixNanos};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Base. The one chain this repository has ever traded on.
pub const BASE: ChainId = ChainId(8453);
pub const EXECUTOR: [u8; 20] = [0x11; 20];
pub const EXECUTOR_VERSION: u32 = 7;
pub const VENUE: VenueId = VenueId(1);

pub fn venue_versions(v: u64) -> BTreeMap<VenueId, u64> {
    let mut m = BTreeMap::new();
    m.insert(VENUE, v);
    m
}

pub fn fingerprint(block: u64, venue_version: u64) -> StateFingerprint {
    StateFingerprint {
        chain_id: BASE,
        parent_block_hash: B256::repeat_byte(0xaa),
        confirmed_block_number: block,
        preconf_sequence: None,
        flashblock_index: None,
        state_root_or_equivalent: None,
        block_hash_if_available: None,
        state_delta_hash: B256::repeat_byte(0xbb),
        venue_state_version: venue_versions(venue_version),
        external_dependency_fingerprint: None,
    }
}

pub fn costs() -> TotalExecutionCost {
    TotalExecutionCost {
        l2_execution_fee: 40_000,
        l1_data_fee: 260_000,
        priority_fee: 1_000,
        builder_payment: 0,
        sequencer_payment: 0,
        flash_fee: 0,
        dex_fees: 30_000,
        expected_failure_cost: 5_000,
        calldata_bytes: 420,
        compressed_data_estimate: 300,
        gas_limit: GasLimit(400_000),
        gas_used_distribution: GasDistribution {
            p50: GasUsed(240_000),
            p90: GasUsed(290_000),
            p99: GasUsed(330_000),
            max_observed: GasUsed(360_000),
        },
    }
}

/// The size is minted here, in `tests/`, and that is load-bearing.
///
/// `DiscreteRefined::new()` is `pub` only because `apex-econ` has to call it
/// across a crate boundary, and `scripts/ci/no_unearned_discrete_size.sh`
/// (INV-18) is what keeps the witness earned. Its exemption for
/// `crates/*/tests/**` is structural rather than a convenience: an integration
/// test compiles to a separate binary that links the library, so a witness
/// minted here can never be reached from `src/`. The plane therefore *carries*
/// sizes and never invents one — which is the property under test.
pub fn size(amount: u64) -> DiscreteSize {
    DiscreteSize::from_refinement(U256::from(amount), DiscreteRefined::new())
}

pub fn route() -> RouteCommitment {
    let token = |id: u8| TokenId { chain: BASE, address: Address::repeat_byte(id) };
    RouteCommitment {
        hops: vec![RouteHop {
            venue: VENUE,
            pool: PoolId { chain: BASE, address: Address::repeat_byte(0x33) },
            token_in: token(0x01),
            token_out: token(0x02),
            fee_ppm: 500,
        }],
        complexity_cost: ComplexityCost {
            hops: 1,
            external_calls: 2,
            calldata_bytes: 420,
            state_deps: 1,
            tick_crossings: 1,
            hooks: 0,
            gas_estimate: 240_000,
            failure_surface: 0.01,
        },
        route_hash: B256::repeat_byte(0x44),
    }
}

pub fn candidate(id: u64, block: u64, net: i128) -> Candidate {
    Candidate {
        candidate_id: CandidateId(id),
        chain_id: BASE,
        strategy: StrategyId(1),
        venue_set: vec![VENUE],
        route: route(),
        state_fingerprint: fingerprint(block, 1),
        state_age: DurationNanos(2_000_000),
        flash_source: None,
        input_amount: size(1_000_000_000_000_000),
        expected_output: U256::from(1_000_400_000_000_000u64),
        gross_profit: U256::from(400_000_000_000u64),
        dex_fees: U256::from(30_000u64),
        flash_fee: U256::ZERO,
        total_execution_cost: costs(),
        expected_net_profit: net,
        robust_ev: net,
        certificate_status: CertificateStatus::Proven,
        simulation_tier: SimulationTier::Tier2FullEvm,
        capture_probability: 0.6,
        robustness_margin: 0.3,
        deadline: UnixNanos(2_000_000_000),
        submission_policy: SubmissionPolicy::Private,
    }
}

// ---------------------------------------------------------------- ports

/// Hands out a fixed candidate list per event, so a stream of N events with a
/// duplicate produces a known number of distinct commitments.
pub struct FixedSearch {
    pub per_event: Vec<Candidate>,
    pub calls: AtomicU64,
}

impl FixedSearch {
    pub fn new(per_event: Vec<Candidate>) -> Self {
        Self { per_event, calls: AtomicU64::new(0) }
    }
}

#[async_trait::async_trait]
impl CandidateSource for FixedSearch {
    async fn candidates(&self, ev: &StateEvent) -> Vec<Candidate> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.per_event
            .iter()
            .map(|c| {
                let mut c = c.clone();
                c.state_fingerprint = ev.fingerprint.clone();
                c
            })
            .collect()
    }
}

/// Returns the candidate's own numbers back. The point of a passthrough here is
/// that the plane must still *join* four independent answers — if it read the
/// candidate directly instead, the concurrency test could not tell.
pub struct PassThroughEconomics;

#[async_trait::async_trait]
impl Economics for PassThroughEconomics {
    async fn reprice(&self, c: &Candidate) -> Result<U256, Decline> {
        Ok(c.expected_output)
    }
    async fn size(&self, c: &Candidate) -> Result<DiscreteSize, Decline> {
        Ok(c.input_amount)
    }
    async fn scenarios(&self, c: &Candidate) -> Result<f64, Decline> {
        Ok(c.robustness_margin)
    }
    async fn refresh_costs(&self, c: &Candidate) -> Result<TotalExecutionCost, Decline> {
        Ok(c.total_execution_cost.clone())
    }
}

/// Declines at whichever of the four stages is named. One stage at a time, so a
/// test can pin which rejection reaches the miss ledger.
pub struct DecliningEconomics(pub Decline);

#[async_trait::async_trait]
impl Economics for DecliningEconomics {
    async fn reprice(&self, c: &Candidate) -> Result<U256, Decline> {
        Ok(c.expected_output)
    }
    async fn size(&self, _c: &Candidate) -> Result<DiscreteSize, Decline> {
        Err(self.0.clone())
    }
    async fn scenarios(&self, c: &Candidate) -> Result<f64, Decline> {
        Ok(c.robustness_margin)
    }
    async fn refresh_costs(&self, c: &Candidate) -> Result<TotalExecutionCost, Decline> {
        Ok(c.total_execution_cost.clone())
    }
}

pub struct AlwaysSucceeds;

#[async_trait::async_trait]
impl Simulator for AlwaysSucceeds {
    async fn simulate(&self, c: &Candidate) -> Result<SimulationResult, Decline> {
        let mut r = SimulationResult {
            tier: SimulationTier::Tier2FullEvm,
            success: true,
            revert: None,
            gas_used: 240_000,
            balance_deltas: BTreeMap::new(),
            loan_repaid: true,
            profit_invariant_held: true,
            token_residues: BTreeMap::new(),
            state_after: c.state_fingerprint.clone(),
            simulated_at_state: c.state_fingerprint.clone(),
            result_hash: B256::ZERO,
            elapsed: DurationNanos(3_000_000),
        };
        r.result_hash = r.canonical_hash();
        Ok(r)
    }
}

pub struct AlwaysAdmits;

impl RiskGate for AlwaysAdmits {
    fn admit(&self, _c: &Candidate, _sim: &SimulationResult) -> Result<(), Decline> {
        Ok(())
    }
}

/// Signs by echoing the commitment hash. A real signer is `apex-exec` plus a
/// key; what matters to the plane is that the bytes it dispatches are the bytes
/// this produced, for the commitment it was handed (INV-06, INV-10).
pub struct EchoSigner;

impl Signer for EchoSigner {
    fn sign(
        &self,
        auth: &apex_capture::revalidate::SigningAuthorization,
        commitment: &apex_types::commitment::ExecutionCommitment,
        gas_limit: GasLimit,
    ) -> Result<SignedPayload, Decline> {
        let hash = commitment.hash();
        Ok(SignedPayload {
            chain: commitment.chain_id,
            hash,
            nonce: auth.nonce().get(),
            gas_limit,
            raw: hash.to_vec(),
        })
    }
}

/// The live side of §24.6's eleven checks. Fixed, so a revalidation failure in
/// a test is always the one the test set up.
pub struct FixedReadings(pub LiveReadings);

#[async_trait::async_trait]
impl LiveReader for FixedReadings {
    async fn read(&self, _t: &OpportunityTicket) -> Result<LiveReadings, Decline> {
        Ok(self.0.clone())
    }
}

pub fn readings() -> LiveReadings {
    LiveReadings {
        executor: EXECUTOR,
        executor_version: EXECUTOR_VERSION,
        observed_fee_wei: 200_000,
        fee_ceiling_wei: 5_000_000,
        remaining_block_gas: 20_000_000,
        signer_balance_wei: 1_000_000_000_000_000_000,
        flash_required: 0,
        flash_available: 0,
        live_hooks: None,
        live_venue_versions: venue_versions(1),
        chain_pending_nonce: 0,
        min_profit: 1,
    }
}

/// A chain that answers all eleven §20 questions. Implementing all of them is
/// the point of `ChainExecutionAdapter` having no defaults — a double that
/// could skip one is a chain that could silently inherit another's economics.
pub struct FakeChain {
    pub landed: bool,
    pub observed: AtomicU64,
    pub reconciled: AtomicU64,
}

impl FakeChain {
    pub fn landing() -> Self {
        Self { landed: true, observed: AtomicU64::new(0), reconciled: AtomicU64::new(0) }
    }
}

#[async_trait::async_trait]
impl ChainExecutionAdapter for FakeChain {
    fn chain_id(&self) -> ChainId {
        BASE
    }
    fn regime(&self, now: UnixNanos) -> AdapterResult<ChainRegime> {
        base_regime(now)
    }
    async fn state_feed(&self) -> AdapterResult<StateFeedHandle> {
        Ok(StateFeedHandle { chain: BASE, feed: "fixture", opened_at: UnixNanos(0) })
    }
    async fn pending_state(&self) -> AdapterResult<PendingState> {
        Ok(PendingState {
            fingerprint: fingerprint(1, 1),
            observed_at: UnixNanos(0),
            flashblock_index: None,
        })
    }
    async fn simulate(&self, _p: &SignedPayload) -> AdapterResult<SimulationResult> {
        Err(apex_chain::adapter::AdapterError::NotSupportedOnThisChain {
            what: "the fixture simulates through apex-sim, not the node",
        })
    }
    fn estimate_total_fee(&self, _c: &Candidate) -> AdapterResult<TotalExecutionCost> {
        Ok(costs())
    }
    fn estimate_inclusion_probability(&self, _c: &Candidate, _at: UnixNanos) -> f64 {
        0.6
    }
    fn optimize_submission_cost(&self, c: &Candidate, _now: UnixNanos) -> SubmissionDecision {
        SubmissionDecision::Submit {
            lane: SubmissionLaneId(1),
            gas_limit: c.total_execution_cost.gas_limit,
            earliest_eligible_flashblock: Some(0),
            max_fee_per_gas_wei: 2_000_000,
            max_priority_fee_per_gas_wei: 100_000,
        }
    }
    async fn submit(&self, _s: &SignedPayload, lane: SubmissionLaneId) -> AdapterResult<Ack> {
        Ok(Ack { lane, stage: LifecycleStage::TransportAccepted, at: UnixNanos(0), tx_hash: None })
    }
    fn replacement_policy(&self) -> ReplacementPolicy {
        ReplacementPolicy { supported: true, min_fee_bump_bps: 1_000, max_attempts: 1 }
    }

    async fn observe_outcome(&self, _h: B256) -> AdapterResult<ObservedOutcome> {
        self.observed.fetch_add(1, Ordering::SeqCst);
        Ok(ObservedOutcome {
            landed_by_competitor: None,
            realized_profit_estimate: self.landed.then_some(320_000_000_000),
        })
    }
    async fn reconcile_final_state(&self, _h: B256) -> AdapterResult<PnlAttribution> {
        self.reconciled.fetch_add(1, Ordering::SeqCst);
        Ok(PnlAttribution {
            ticket_id: apex_types::ids::TicketId(0),
            chain: BASE,
            strategy: StrategyId(1),
            venues: vec![VENUE],
            route_hash: B256::repeat_byte(0x44),
            optimization_layers: vec![OptimizationLayer::SinglePath],
            gross_profit: 400_000_000_000,
            realized_cost: costs(),
            net_profit_token: 320_000_000_000,
            net_profit_usd_bounds: UsdBounds { low: 0.9, high: 1.1 },
        })
    }
}

/// A chain that cannot answer. Boot reconciliation must refuse rather than
/// conclude "it did not land".
pub struct SilentChain;

impl ChainOutcomeSource for SilentChain {
    fn resolve(&self, _t: &OpportunityTicket) -> Result<TicketOutcome, String> {
        Err("no receipt endpoint in this fixture".to_string())
    }
}

/// Nothing outstanding to resolve, so boot succeeds on an empty journal.
pub struct NoChain;

impl ChainOutcomeSource for NoChain {
    fn resolve(&self, t: &OpportunityTicket) -> Result<TicketOutcome, String> {
        Ok(TicketOutcome::ExplicitFailure {
            code: TerminalFailure::Abandoned { at_status: TicketStatus::Signed },
            at: UnixNanos(0),
            state: Box::new(t.state_fingerprint.clone()),
            cause: "fixture: resolved as never included".to_string(),
        })
    }
}

/// Base's regime, discovered rather than assumed — there is no other way to get
/// a `ChainRegime`, which is `apex-chain`'s point and holds here too.
pub fn base_regime(now: UnixNanos) -> AdapterResult<ChainRegime> {
    use apex_chain::regime::{
        FeeModel, OrderingMode, PriorityFeeSemantics, RegimeDiscovery, ReplacementRules,
    };
    RegimeDiscovery::discovered(
        BASE,
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
        now,
    )
    .admit_to_live_trading(now, DurationNanos(60_000_000_000))
    .copied()
    .map_err(|e| apex_chain::adapter::AdapterError::Uninterpretable { detail: e.to_string() })
}

/// Reads the recorded stream from `tests/fixtures/`.
pub fn recorded_stream() -> Vec<StateEvent> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/recorded_stream.json");
    let text = std::fs::read_to_string(path).expect("the recorded stream fixture");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    let events = doc.get("events").expect("an events array").clone();
    serde_json::from_value(events).expect("events matching StateEvent")
}

pub fn pending_swap(target: u8) -> EventKind {
    EventKind::PendingSwap { target: B256::repeat_byte(target) }
}

/// The four §46.2 stages, each waiting on a 4-way barrier. A serial join
/// deadlocks here; see `workers::independent_tasks_run_concurrently`.
pub struct BarrieredEconomics {
    pub barrier: Arc<tokio::sync::Barrier>,
}

#[async_trait::async_trait]
impl Economics for BarrieredEconomics {
    async fn reprice(&self, c: &Candidate) -> Result<U256, Decline> {
        self.barrier.wait().await;
        Ok(c.expected_output)
    }
    async fn size(&self, c: &Candidate) -> Result<DiscreteSize, Decline> {
        self.barrier.wait().await;
        Ok(c.input_amount)
    }
    async fn scenarios(&self, c: &Candidate) -> Result<f64, Decline> {
        self.barrier.wait().await;
        Ok(c.robustness_margin)
    }
    async fn refresh_costs(&self, c: &Candidate) -> Result<TotalExecutionCost, Decline> {
        self.barrier.wait().await;
        Ok(c.total_execution_cost.clone())
    }
}

/// A ticket at a given status, for tests that need one without running a plane.
pub fn ticket_at(status: TicketStatus) -> OpportunityTicket {
    use apex_types::ticket::ExecutionWindow;
    OpportunityTicket {
        ticket_id: apex_types::ids::TicketId(0),
        chain_id: BASE,
        strategy: StrategyId(1),
        state_fingerprint: fingerprint(47_079_437, 1),
        route_commitment: route(),
        exact_input: U256::from(1_000_000_000_000_000u64),
        expected_net_ev: 320_000_000_000,
        robustness_margin: 0.3,
        validity_start: UnixNanos(1_000_000_000),
        dispatch_deadline: UnixNanos(9_000_000_000),
        target_execution_window: ExecutionWindow {
            earliest: UnixNanos(1_000_000_000),
            latest: UnixNanos(9_000_000_000),
            earliest_eligible_flashblock: Some(0),
        },
        signer_lane: None,
        nonce: None,
        flash_source: None,
        simulation_result_hash: B256::ZERO,
        submission_policy: SubmissionPolicy::Private,
        required_gas_limit: GasLimit(400_000),
        created_at: UnixNanos(1_000_000_000),
        status,
    }
}




/// §25's commitment, as `apex-exec` will build it.
///
/// The one field the plane genuinely cannot supply is `venue_fingerprints`: it
/// comes from the venue adapters, is not on `Candidate`, and deriving it from the
/// state versions would be inventing the value the executor independently
/// recomputes. That is why `Commitments` is a port and not a function.
pub struct FixtureCommitments;

impl Commitments for FixtureCommitments {
    fn commit(
        &self,
        c: &Candidate,
        auth: &apex_capture::signer::ExecutorAuth,
        min_profit: U256,
    ) -> Result<apex_types::commitment::ExecutionCommitment, Decline> {
        let mut venue_fingerprints = BTreeMap::new();
        for (venue, version) in &c.state_fingerprint.venue_state_version {
            venue_fingerprints.insert(*venue, B256::from(U256::from(*version)));
        }
        // The auth carries a 32-byte version; the commitment and the last-mile
        // context both use a `u32`. Taking the low four bytes rather than
        // hashing, so the two representations of one version stay comparable by
        // eye in a journal.
        let mut v = [0u8; 4];
        v.copy_from_slice(&auth.executor_version[28..]);
        Ok(apex_types::commitment::ExecutionCommitment {
            chain_id: c.chain_id,
            executor_address: Address::from(auth.executor),
            executor_version: u32::from_be_bytes(v),
            venue_fingerprints,
            flash_source: c
                .flash_source
                .as_ref()
                .map_or(apex_types::ids::FlashProviderId(0), |f| f.provider),
            state_fingerprint_hash: c.state_fingerprint.state_delta_hash,
            route_hash: c.route.route_hash,
            exact_inputs: vec![c.input_amount.get()],
            min_profit,
            slippage_constraints: vec![30],
            deadline: c.deadline.0 / 1_000_000_000,
            submission_policy: c.submission_policy,
        })
    }
}

fn receipt(tx: B256) -> apex_chain::base::observe::Receipt {
    apex_chain::base::observe::Receipt {
        tx,
        block_number: 47_079_440,
        success: true,
        gas_used: 240_000,
        effective_gas_price_wei: 6_000_000,
        l1_fee_wei: 1_369_291_442,
        }
}

/// Observations reaching the whole ladder: preconfirmed, then included, then
/// finalized.
pub struct LandsAndFinalizes;

#[async_trait::async_trait]
impl SettlementFeed for LandsAndFinalizes {
    async fn observe(
        &self,
        tx: B256,
    ) -> Result<Vec<apex_chain::base::observe::TransactionObservation>, Decline> {
        use apex_chain::base::observe::TransactionObservation as O;
        Ok(vec![
            O::preconfirmed(tx, UnixNanos(1_000_000_100)),
            O::included(receipt(tx), UnixNanos(1_000_000_200)),
            O::finalized(receipt(tx), UnixNanos(1_000_000_300)),
        ])
    }
}

/// A chain with no preconfirmation window: the transaction is included, and
/// `Preconfirmed` is never observed.
///
/// The plane must therefore never record `Preconfirmed` for it. That is the test
/// that proves the status walk is driven by evidence rather than by a list.
pub struct IncludesWithoutPreconfirming;

#[async_trait::async_trait]
impl SettlementFeed for IncludesWithoutPreconfirming {
    async fn observe(
        &self,
        tx: B256,
    ) -> Result<Vec<apex_chain::base::observe::TransactionObservation>, Decline> {
        use apex_chain::base::observe::TransactionObservation as O;
        Ok(vec![O::included(receipt(tx), UnixNanos(1_000_000_200))])
    }
}

/// Nothing ever landed. §16.2 step 9: reprice or explicitly abandon.
pub struct NeverLands;

#[async_trait::async_trait]
impl SettlementFeed for NeverLands {
    async fn observe(
        &self,
        _tx: B256,
    ) -> Result<Vec<apex_chain::base::observe::TransactionObservation>, Decline> {
        Ok(Vec::new())
    }
}

/// Records the ticket status the registry held **at the moment it was called**.
///
/// This is how the write-ahead ordering is tested. `recover::scan` divides crash
/// survivors on whether the journal saw `Signed`, and that division is sound only
/// if `advance(Signed)` is journalled *before* the signer runs. Nothing about the
/// recorded status *sequence* can show the interleaving; asking the signer what it
/// saw can.
pub struct StatusWatchingSigner {
    pub registry: Arc<apex_capture::registry::TicketRegistry>,
    pub seen: std::sync::Mutex<Vec<(&'static str, Option<TicketStatus>)>>,
}

impl StatusWatchingSigner {
    pub fn new(registry: Arc<apex_capture::registry::TicketRegistry>) -> Self {
        Self { registry, seen: std::sync::Mutex::new(Vec::new()) }
    }
    pub fn seen(&self) -> Vec<(&'static str, Option<TicketStatus>)> {
        self.seen.lock().map_or_else(|p| p.into_inner().clone(), |g| g.clone())
    }
    fn note(&self, what: &'static str, id: apex_types::ids::TicketId) {
        let status = self.registry.status(id);
        if let Ok(mut g) = self.seen.lock() {
            g.push((what, status));
        }
    }
}

impl Signer for StatusWatchingSigner {
    fn sign(
        &self,
        auth: &apex_capture::revalidate::SigningAuthorization,
        commitment: &apex_types::commitment::ExecutionCommitment,
        gas_limit: GasLimit,
    ) -> Result<SignedPayload, Decline> {
        self.note("sign", auth.ticket());
        EchoSigner.sign(auth, commitment, gas_limit)
    }
}

/// The dispatcher half of the same question.
pub struct StatusWatchingDispatcher {
    pub registry: Arc<apex_capture::registry::TicketRegistry>,
    pub seen: std::sync::Mutex<Vec<(&'static str, Option<TicketStatus>)>>,
}

impl StatusWatchingDispatcher {
    pub fn new(registry: Arc<apex_capture::registry::TicketRegistry>) -> Self {
        Self { registry, seen: std::sync::Mutex::new(Vec::new()) }
    }
    pub fn seen(&self) -> Vec<(&'static str, Option<TicketStatus>)> {
        self.seen.lock().map_or_else(|p| p.into_inner().clone(), |g| g.clone())
    }
}

impl apex_capture::dispatch::Dispatcher for StatusWatchingDispatcher {
    fn dispatch(
        &self,
        req: &apex_capture::dispatch::DispatchRequest,
        permit: &apex_capture::recover::DispatchPermit<'_>,
        now: UnixNanos,
    ) -> Result<apex_capture::dispatch::AckLadder, apex_capture::dispatch::DispatchError> {
        let status = self.registry.status(req.ticket());
        if let Ok(mut g) = self.seen.lock() {
            g.push(("dispatch", status));
        }
        apex_capture::NullDispatcher::new().dispatch(req, permit, now)
    }
}
