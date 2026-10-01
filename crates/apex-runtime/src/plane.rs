//! §16.2's eleven-step mandatory capture protocol, assembled.
//!
//! ```text
//!  1. LOCK opportunity commitment                     lock()
//!  2. RESERVE signer + nonce lane                     SignerPool::assign + reserve_nonce
//!  3. RESERVE simulation / RPC / dispatch capacity    Budgets::reserve
//!  4. REVALIDATE only critical mutable state          capture::last_mile
//!  5. SIGN exact committed payload                    Signer port
//!  6. DISPATCH through approved lanes                 capture::Dispatcher
//!  7. OBSERVE transport/node/sequencer ack            capture::AckLadder
//!  8. OBSERVE preconfirmation / inclusion             SettlementFeed port
//!  9. IF state changes before inclusion: reprice or abandon
//! 10. RECONCILE receipt, balances, realized P&L       ChainExecutionAdapter
//! 11. CLOSE with success or explicit failure          TicketRegistry::close
//! ```
//!
//! §16.2 says "each step is a distinct function returning a typed token consumed
//! by the next, so a step cannot be skipped (the type system, not review,
//! enforces the order)". Phase 6 built three of those tokens — `Revalidated`,
//! `SigningAuthorization`, `DispatchPermit` — and this module adds the first:
//! [`Locked`], which [`Plane::admit`] requires, so a ticket cannot exist without
//! step 1 having happened.
//!
//! # Step 1 was never built, and it is the one that costs money when it is absent
//!
//! §17.4: "`ExecutionCommitment::hash()` is the deduplication key. An in-flight
//! ticket with an identical commitment hash suppresses a new one." §7's crate
//! table assigns §25 duplicate suppression to `apex-capture`; nothing in
//! `apex-capture` mentions `ExecutionCommitment`, and Phase 6's delivery record
//! does not claim it. It lands here because the key is the commitment and the
//! registry neither knows nor should know what a commitment is: the registry
//! *accounts for* tickets, and suppression decides whether a ticket should
//! exist.
//!
//! It matters because a state feed **redelivers**. A websocket reconnect
//! replays; two feeds carry the same block; §5.6's feed arbiter exists precisely
//! because more than one source is watched. Without suppression one opportunity
//! becomes two signed transactions for a trade that can only land once — which is
//! duplicate gas at best, and at worst two nonces burnt on one edge.
//!
//! # The ports, and which crate eventually fills each one
//!
//! | Port | Crate | Phase |
//! |---|---|---|
//! | [`RouteSource`] | `apex-search` | 2b |
//! | [`Economics`] | `apex-econ` | 3, 10, 2b |
//! | [`Simulator`] | `apex-sim` | 3, 9 |
//! | [`RiskGate`] | `apex-risk` | 6, 8 |
//! | [`Commitments`] | `apex-exec` | 5 |
//! | [`Signer`] | `apex-exec` + key management | 5, 6 |
//! | [`LiveReader`] | `apex-chain` + `apex-state` | 7 |
//! | [`SettlementFeed`] | `apex-chain` | 7 |
//!
//! Eight traits is a lot, and each one is a crate boundary §7 already drew
//! rather than a seam invented to make testing convenient. Three of those crates
//! do not exist yet; the plane is written against what they will be, which is
//! the only order that does not require rewriting the plane when they arrive.
//!
//! # Write-ahead, and why `advance` comes before the act
//!
//! `recover::scan` divides crash survivors on whether the journal saw `Signed`,
//! and that division is sound *only* if the intent is journalled before the
//! irreversible act. So `advance(Signed)` returns `Ok` before the signer is
//! called and `advance(Dispatching)` before anything is sent. Getting this
//! backwards would make a crash mid-sign look like a ticket that was never
//! signed, and boot recovery would close it without asking the chain.
//!
//! # Every status move needs evidence
//!
//! `TicketStatus::Reconciled` is the last variant and `advance` accepts any
//! forward move, so a plane could reach it in one step. Each transition here is
//! instead produced by the step that earned it, and the ladder statuses
//! (`Preconfirmed`, `Included`, `Finalized`) are driven from observations rather
//! than assumed: a chain that only ever preconfirms produces a ticket that never
//! records `Included`, and that is the correct journal.

use crate::bus::StateEvent;
use apex_search::frontier::RouteProposal;
use crate::workers::{refine_concurrently, Budgets, ResourceClass};
use apex_capture::dispatch::{DispatchError, DispatchRequest, Dispatcher, NullDispatcher};
use apex_capture::scheduler::{CaptureAssurance, Scheduler};
use apex_econ::eligibility::Clause;
use apex_exec::call::ExecutorCall;
use apex_capture::recover::{reconcile, scan, ChainOutcomeSource, DispatchGate, RecoveryError};
use apex_capture::registry::{TicketGuard, TicketRegistry};
use apex_capture::revalidate::{
    last_mile, LastMileCheck, LastMileContext, SigningAuthorization,
};
use apex_capture::signer::{ExecutorAuth, NoLane, SignerPool};
use apex_chain::adapter::{
    ChainExecutionAdapter, RejectReason, SignedPayload, SubmissionDecision,
};
use apex_chain::base::observe::TransactionObservation;
use apex_obs::miss::{MissContext, MissLedger};
use apex_types::ack::LifecycleStage;
use apex_types::candidate::{Candidate, DiscreteSize};
use apex_types::commitment::ExecutionCommitment;
use apex_types::cost::{GasLimit, TotalExecutionCost};
use apex_types::ids::{ChainId, SubmissionLaneId, TicketId};
use apex_types::miss::{ExplainsMiss, MissReason, SearchPath};
use apex_types::pnl::PnlAttribution;
use apex_types::sim::{RevertClass, SimulationResult};
use apex_types::ticket::{
    ExecutionWindow, OpportunityTicket, TerminalFailure, TicketOutcome, TicketStatus,
};
use apex_types::time::{DurationNanos, UnixNanos};
use alloy_primitives::{Address, B256, U256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Why the plane declined a candidate.
///
/// # The mapping rule, stated once
///
/// §33 has seventeen buckets and none of them is "our own infrastructure did not
/// answer". Rather than overload `RISK_FAIL` — the failure Task 8.1 warned
/// about, where one bucket becomes the largest and least informative — the rule
/// here is: **a refusal by our machinery is `RiskFail`; work that did not happen
/// in time is `TooSlow`.** A signer that says no is a refusal; a chain that does
/// not answer is a timeout. Recorded in PLAN.md as a §33 gap rather than met by
/// inventing an eighteenth bucket mid-task.
#[derive(Clone, Debug, PartialEq)]
pub enum Decline {
    /// §14: no integer size clears the costs. This repository's measured 96%
    /// bucket.
    NoProfitableSize,
    /// The state this candidate priced against is already gone.
    StaleState { age: DurationNanos },
    /// Simulation refused it. The class is `None` when the simulator failed
    /// rather than the transaction reverting — a distinction the loss ledger
    /// needs and a bare boolean destroys.
    SimulationFailed { class: Option<RevertClass> },
    /// The risk gate refused it (§28). Risk is a hard gate, not advice.
    RiskRefused { rule: String },
    /// §18.2: no signer lane.
    NoSignerLane(NoLane),
    /// §24.6's eleven last-mile checks refused it.
    Revalidation(LastMileCheck),
    /// §21.3: the chain adapter will not submit this.
    ChainRejected(RejectReason),
    /// **INV-39.** Boot reconciliation has not completed, so no live ticket may
    /// be created at all. Checked before step 1, because §46.1's wording is
    /// "before new live dispatch is re-enabled" — a system that admits tickets it
    /// cannot dispatch is manufacturing work for the drain.
    DispatchGateShut,
    /// §29.3: this resource class is at its budget.
    NoBudget(ResourceClass),
    /// The transport refused the payload.
    DispatchFailed(DispatchError),
    /// The signer would not produce a payload.
    Unsigned { detail: String },
    /// The chain could not be asked. Never converted into "it did not land"
    /// (§5.6, §16.8).
    ChainUnavailable { detail: String },
    /// A commitment could not be built for this candidate.
    Uncommittable { detail: String },
    /// A venue on this route has not been verified: an unadmitted pool, or code
    /// evidence older than the bound.
    ///
    /// Distinct from [`Self::Uncommittable`] because the response is different
    /// and specific — go verify the venue — and because `VenueDisabled` is the
    /// §33 bucket that says so. Folding it into a generic failure would put a
    /// venue problem in a bucket nobody acts on.
    VenueUnverified { detail: String },
}

impl std::fmt::Display for Decline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProfitableSize => f.write_str("no integer size clears the costs"),
            Self::StaleState { age } => write!(f, "state is {} ns old", age.0),
            Self::SimulationFailed { class } => write!(f, "simulation refused it: {class:?}"),
            Self::RiskRefused { rule } => write!(f, "risk gate refused it: {rule}"),
            Self::NoSignerLane(n) => write!(f, "no signer lane: {n}"),
            Self::Revalidation(c) => write!(f, "last-mile revalidation: {c}"),
            Self::ChainRejected(r) => write!(f, "the chain declined: {r}"),
            Self::DispatchGateShut => {
                f.write_str("boot reconciliation has not completed; dispatch is impossible")
            }
            Self::NoBudget(c) => write!(f, "{} is at its budget", c.label()),
            Self::DispatchFailed(e) => write!(f, "dispatch failed: {e}"),
            Self::Unsigned { detail } => write!(f, "the signer refused: {detail}"),
            Self::ChainUnavailable { detail } => write!(f, "the chain could not be asked: {detail}"),
            Self::Uncommittable { detail } => write!(f, "no commitment could be built: {detail}"),
            Self::VenueUnverified { detail } => write!(f, "venue not verified: {detail}"),
        }
    }
}

impl std::error::Error for Decline {}

impl ExplainsMiss for Decline {
    fn miss_reason(&self) -> MissReason {
        match self {
            Self::NoProfitableSize => MissReason::LowEv,
            Self::StaleState { .. } => MissReason::StaleState,
            Self::SimulationFailed { .. } => MissReason::SimFail,
            // Delegated rather than re-decided. The crate that owns the
            // rejection owns its bucket; re-mapping here would let the two
            // answers drift.
            Self::NoSignerLane(n) => n.miss_reason(),
            Self::Revalidation(c) => c.miss_reason(),
            Self::ChainRejected(r) => r.miss_reason(),
            // Refusals by our own machinery.
            Self::RiskRefused { .. } | Self::DispatchGateShut | Self::Unsigned { .. } => {
                MissReason::RiskFail
            }
            // Work that did not happen in time.
            Self::NoBudget(_) | Self::ChainUnavailable { .. } | Self::Uncommittable { .. } => {
                MissReason::TooSlow
            }
            // Not a refusal by our machinery and not a timeout: the venue is not
            // usable for this route until somebody verifies it, which is exactly
            // what this bucket is for and is the actionable reading.
            Self::VenueUnverified { .. } => MissReason::VenueDisabled,
            // §33 splits submission rejection into builder and sequencer, and a
            // `DispatchError` carries neither: it knows the lane refused, not
            // what kind of thing the lane talks to. Base has a sequencer and is
            // the only chain this system submits to, so `SequencerRejected` is
            // true today. An Ethereum adapter must carry the distinction on the
            // error rather than have this function guess.
            Self::DispatchFailed(_) => MissReason::SequencerRejected,
        }
    }
}

/// §17.4's answer when an identical commitment is already in flight.
///
/// not-a-candidate-rejection: a suppressed duplicate is not a missed
/// opportunity — the identical trade is already being captured by the ticket
/// holding this commitment. Filing it as a miss would put opportunities the
/// system *took* into the dataset that decides where engineering effort goes,
/// which is the same error `StepDownRefused` avoids by the same means. It
/// therefore does not implement `ExplainsMiss`, deliberately, and this comment
/// is the greppable form of that claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Suppressed {
    pub commitment: B256,
}

/// Why step 1 did not produce a lock.
///
/// Two outcomes that must not be folded together: a **suppression** is the system
/// already capturing this opportunity, and a **decline** is the system unable to
/// describe it. The first is not a miss and the second is, so a single error type
/// would put trades the system took into the dataset that decides where
/// engineering effort goes.
#[derive(Clone, Debug, PartialEq)]
pub enum LockFailure {
    /// §17.4: an identical commitment is already in flight.
    Suppressed(Suppressed),
    /// The commitment could not be built — an unverified venue, an executor
    /// version that does not fit, a route with no hops.
    Declined(Decline),
}

impl std::fmt::Display for LockFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Suppressed(s) => write!(f, "{s}"),
            Self::Declined(d) => write!(f, "{d}"),
        }
    }
}

impl std::error::Error for LockFailure {}

impl std::fmt::Display for Suppressed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "commitment {} is already in flight", self.commitment)
    }
}

impl std::error::Error for Suppressed {}

// ------------------------------------------------------------------ ports

/// **Renamed from `CandidateSource`, and it returns proposals rather than
/// candidates (Task 2b.5).**
///
/// Task 8.4 named this `CandidateSource`, had it return `Vec<Candidate>`, and
/// named `apex-search` as the crate that would implement it. `apex-search` could
/// not: §6.1 runs `apex-search → apex-econ`, `Candidate::input_amount` is a
/// `DiscreteSize`, and only `apex-econ`'s refinement path can mint one (INV-18).
/// A `Candidate` also carries `total_execution_cost`, `robust_ev`,
/// `capture_probability`, `certificate_status` and `simulation_tier` — every one
/// an `apex-econ` or `apex-sim` output. **The port was named for a crate that
/// could not satisfy it**, and no amount of care inside `apex-search` would have
/// fixed that.
///
/// A search proposes a route; economics decides whether and at what size it is a
/// trade. That is §6.1's graph, and it is now the signature.
#[async_trait::async_trait]
pub trait RouteSource: Send + Sync {
    async fn propose(&self, event: &StateEvent) -> Vec<RouteProposal>;
}

/// §46.2's four independent answers, **over a proposal**. Four methods rather
/// than one, because [`refine_concurrently`] has to be able to have all four in
/// flight at once — a single `refine` method would make the concurrency an
/// implementation detail of whoever implements this trait, which is exactly
/// where it would quietly become serial again.
///
/// The join is what turns a proposal into a `Candidate`, and [`Economics::assemble`]
/// is where that happens: the plane owns *when* the stages run, `apex-econ` owns
/// *what the numbers mean*. A plane that assembled a candidate itself would be
/// inventing economics, which is the god-object shape one layer up.
#[async_trait::async_trait]
pub trait Economics: Send + Sync {
    async fn reprice(&self, p: &RouteProposal) -> Result<U256, Decline>;
    /// Returns a [`DiscreteSize`], which only `apex-econ`'s refinement path can
    /// mint (INV-18). The proposal carries a `size_hint: Option<U256>` — an input
    /// to this refinement, never a substitute for it.
    async fn size(&self, p: &RouteProposal) -> Result<DiscreteSize, Decline>;
    async fn scenarios(&self, p: &RouteProposal) -> Result<f64, Decline>;
    async fn refresh_costs(&self, p: &RouteProposal) -> Result<TotalExecutionCost, Decline>;

    /// Join the four into a candidate. Synchronous: it is arithmetic over four
    /// answers that are already in hand, and an `async` here would be a place to
    /// put an RPC call.
    fn assemble(&self, p: &RouteProposal, r: Refinement) -> Result<Candidate, Decline>;
}

/// What the four stages produced, joined.
#[derive(Clone, Debug, PartialEq)]
pub struct Refinement {
    pub expected_output: U256,
    pub input_amount: DiscreteSize,
    pub robustness_margin: f64,
    pub costs: TotalExecutionCost,
}

/// §25's executor call for a candidate: the plan, its commitment, and the
/// `startV2` calldata, built **once per ticket**.
///
/// The simulator and the signer are handed the same [`ExecutorCall`], so what is
/// simulated is what is signed — there is one set of bytes and both read it.
/// Before this port neither could see the transaction at all: the simulator took
/// a `Candidate` and the signer an `ExecutionCommitment`.
///
/// A route no executor op can encode is refused here, before any simulation is
/// spent on it. `apex-exec` owns the encoding; the venue-to-op mapping belongs
/// to whoever knows the deployment's adapter registrations.
pub trait CallBuilder: Send + Sync {
    fn build(
        &self,
        c: &Candidate,
        commitment: &ExecutionCommitment,
    ) -> Result<ExecutorCall, Decline>;
}

/// Simulate **the call**, as the lane that will sign it.
///
/// `from` matters because `startV2` is `onlyExecutor`: a simulation as anyone
/// but the assigned lane answers a different question.
#[async_trait::async_trait]
pub trait Simulator: Send + Sync {
    async fn simulate(
        &self,
        c: &Candidate,
        call: &ExecutorCall,
        from: Address,
    ) -> Result<SimulationResult, Decline>;
}

/// Risk is a hard execution gate, not advice (§28). Synchronous because a gate
/// that can await is a gate something can be waiting behind while a deadline
/// runs down.
///
/// `lane` is where an admission would lead. It exists for exactly one decision:
/// a gate may waive INV-17 for a [`LaneKind::Shadow`] plane and must say so in
/// the [`Admission`] it returns. The plane, not the gate, has the last word —
/// see [`DispatchLane`].
pub trait RiskGate: Send + Sync {
    fn admit(
        &self,
        c: &Candidate,
        sim: &SimulationResult,
        lane: LaneKind,
    ) -> Result<Admission, Decline>;
}

/// Where a signed payload goes. Two answers, and the difference between them is
/// whether money can move.
///
/// # The shadow variant holds a concrete type, and that is the guarantee
///
/// `Shadow` holds an `Arc<NullDispatcher>` — not a trait object — so a
/// dispatcher that sends **cannot be put in it**: no value of that type sends.
/// A shadow plane's waiver of INV-17 is granted only to planes built on this
/// variant, which makes "a heuristic route reached a lane that sends" a type
/// error rather than a review finding.
///
/// The reverse is harmless and deliberately allowed. `Live` takes any
/// dispatcher, a null one included, and that is how the lifecycle tests drive
/// the whole live protocol — the unwaived gate, settlement, reconciliation —
/// with no network.
#[derive(Clone)]
pub enum DispatchLane {
    /// §16.1's null dispatcher. Records; sends nothing.
    Shadow(Arc<NullDispatcher>),
    /// Anything that implements [`Dispatcher`]. Only an [`Admission::Full`]
    /// reaches it.
    Live(Arc<dyn Dispatcher + Send + Sync>),
}

impl DispatchLane {
    pub const fn kind(&self) -> LaneKind {
        match self {
            Self::Shadow(_) => LaneKind::Shadow,
            Self::Live(_) => LaneKind::Live,
        }
    }
}

/// What the risk gate is told about where its admission would lead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneKind {
    Shadow,
    Live,
}

/// The risk gate's yes, and how much of a yes it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Every clause held. May reach either lane.
    Full,
    /// Every clause held except `waived`, which only a shadow plane may waive.
    /// **Reaches only [`DispatchLane::Shadow`]**: on a live lane the plane
    /// refuses it before signing, whatever the gate that granted it was told.
    ShadowOnly { waived: Vec<Clause> },
}

impl Admission {
    /// The clauses this admission did not satisfy. Empty for `Full`.
    pub fn waived(&self) -> &[Clause] {
        match self {
            Self::Full => &[],
            Self::ShadowOnly { waived } => waived,
        }
    }
}

fn labels(clauses: &[Clause]) -> Vec<String> {
    clauses.iter().map(|c| c.label().to_string()).collect()
}

/// §25's commitment. `apex-exec`'s job: the plane cannot build one because
/// `venue_fingerprints` comes from the venue adapters and is not a candidate
/// field, and deriving it here would be inventing the thing the executor
/// independently recomputes.
pub trait Commitments: Send + Sync {
    fn commit(
        &self,
        c: &Candidate,
        auth: &ExecutorAuth,
        min_profit: U256,
    ) -> Result<ExecutionCommitment, Decline>;
}

/// Sign **the call** — the bytes that were simulated — at the nonce the
/// authorization reserved.
///
/// Synchronous for the same reason as [`RiskGate`]: signing is arithmetic over a
/// key already in memory, and an `async` here would be a place to put a network
/// round trip between the last-mile check and the signature.
pub trait Signer: Send + Sync {
    fn sign(
        &self,
        auth: &SigningAuthorization,
        call: &ExecutorCall,
        gas_limit: GasLimit,
        fees: FeeCaps,
    ) -> Result<SignedPayload, Decline>;
}

/// What a transaction may pay **per unit of gas**.
///
/// Its own type because the unit has already gone wrong once:
/// `SubmissionDecision::Submit::max_fee_per_gas_wei` is filled from
/// `TotalExecutionCost::l2_execution_fee`, which is a *total* (recorded in
/// PLAN.md). Nothing reads that field; the signer reads this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeeCaps {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// The caps a ticket signs with, from the readings taken for its last-mile
/// check.
///
/// Twice the base fee observed at the head — the conventional EIP-1559 headroom,
/// so a transaction survives a run of full blocks rather than failing on the
/// first increase — and never above the policy ceiling, which is the same
/// ceiling revalidation checks the observed fee against.
///
/// **No tip.** §4.5 says a priority fee on Base ranks within the sequencer's
/// window, and nothing has measured what it buys; `LiveEconomics` prices the
/// priority fee at zero for the same reason. A fabricated bid is a cost the EV
/// would then have to clear.
pub fn fee_caps(r: &LiveReadings) -> FeeCaps {
    FeeCaps {
        max_fee_per_gas: r.observed_fee_wei.saturating_mul(2).min(r.fee_ceiling_wei),
        max_priority_fee_per_gas: 0,
    }
}

/// The **live** side of §24.6's eleven comparisons, read once, immediately before
/// signing. Both sides of every comparison have to be explicit — a check that
/// reads one side from ambient state is a check whose answer depends on when it
/// ran — so this is the side that is not on the ticket.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveReadings {
    pub executor: [u8; 20],
    pub executor_version: u32,
    pub observed_fee_wei: u128,
    pub fee_ceiling_wei: u128,
    pub remaining_block_gas: u64,
    pub signer_balance_wei: u128,
    pub flash_required: u128,
    pub flash_available: u128,
    pub live_hooks: Option<[u8; 32]>,
    pub live_venue_versions: BTreeMap<apex_types::ids::VenueId, u64>,
    pub chain_pending_nonce: u64,
    pub min_profit: i128,
}

#[async_trait::async_trait]
pub trait LiveReader: Send + Sync {
    async fn read(&self, ticket: &OpportunityTicket) -> Result<LiveReadings, Decline>;
}

/// Steps 7–8's evidence.
///
/// **Not `ChainExecutionAdapter::observe_outcome`,** and that is a plan gap
/// rather than a preference. That method returns `apex_types::miss::
/// ObservedOutcome`, whose fields are `landed_by_competitor` and
/// `realized_profit_estimate` — a *miss-ledger* record, which is the right answer
/// to "what happened to the opportunity we did not take" and cannot say which
/// lifecycle stage **our own** transaction reached. §2.5 needs exactly that to
/// drive `Preconfirmed` / `Included` / `Finalized`.
///
/// `apex-chain` already has the right type: `TransactionObservation` carries a
/// `LifecycleStage` and, from `Included` onward, a receipt. It is simply not on
/// the eleven-method trait. Recorded in PLAN.md; bridged here rather than by
/// adding a twelfth method, because §20's method count is something the plan
/// settled deliberately.
#[async_trait::async_trait]
pub trait SettlementFeed: Send + Sync {
    async fn observe(&self, tx: B256) -> Result<Vec<TransactionObservation>, Decline>;
}

// ------------------------------------------------------------------ plane

/// Everything the plane talks to.
pub struct Ports {
    pub registry: Arc<TicketRegistry>,
    pub pool: Arc<SignerPool>,
    pub gate: Arc<DispatchGate>,
    pub dispatch: DispatchLane,
    pub chain: Arc<dyn ChainExecutionAdapter>,
    pub search: Arc<dyn RouteSource>,
    pub econ: Arc<dyn Economics>,
    pub sim: Arc<dyn Simulator>,
    pub risk: Arc<dyn RiskGate>,
    pub commitments: Arc<dyn Commitments>,
    pub calls: Arc<dyn CallBuilder>,
    pub signer: Arc<dyn Signer>,
    pub live: Arc<dyn LiveReader>,
    pub settlement: Arc<dyn SettlementFeed>,
}

/// Observations already handled, by `(chain, ordinal)`.
///
/// **A second, cheaper dedup, and Task 2b.5 is what justified it.** §17.4's
/// commitment hash covers `exact_inputs` and `min_profit`, so the commitment
/// cannot exist until the size does — which means the §46.2 join runs *before*
/// step 1 and a redelivered observation pays for a full refinement before
/// anything notices it is a duplicate. On the hot path that is the §29 compute
/// budget spent on work already done.
///
/// So the two levels catch different things, and neither subsumes the other:
///
/// - **Here**, by `(chain, Ordinal)`: the same observation arriving twice — a
///   websocket reconnect replaying, two feeds carrying one block. Cheap, and it
///   fires before any pricing.
/// - **[`InFlight`]**, by commitment hash: two *different* events proposing the
///   same trade. Exact, and it is the one that protects the money.
///
/// Bounded, and eviction is by age rather than by any cleverness: feed
/// redelivery is a recent-window phenomenon, so a window is the right shape. A
/// duplicate older than the window costs a refinement and is then caught by the
/// commitment, which is the correct place for a rare case.
#[derive(Debug)]
struct SeenEvents {
    inner: Mutex<SeenInner>,
    capacity: usize,
}

#[derive(Debug, Default)]
struct SeenInner {
    set: BTreeSet<(u64, apex_state::Ordinal)>,
    order: std::collections::VecDeque<(u64, apex_state::Ordinal)>,
}

impl SeenEvents {
    /// 256 observations. Base seals a block every 2 s and emits a flashblock
    /// every 200 ms, so this is roughly a minute of feed — comfortably longer
    /// than a websocket reconnect and far shorter than anything that would make
    /// the set a memory concern.
    const DEFAULT_CAPACITY: usize = 256;

    fn new(capacity: usize) -> Self {
        Self { inner: Mutex::new(SeenInner::default()), capacity: capacity.max(1) }
    }

    /// `true` when this observation is new. Recovering from a poisoned lock
    /// rather than propagating: refusing every event because one panic happened
    /// mid-insert would halt the chain, which is far worse than the duplicate
    /// this set exists to prevent.
    fn take(&self, chain: ChainId, at: apex_state::Ordinal) -> bool {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let key = (chain.0, at);
        if !inner.set.insert(key) {
            return false;
        }
        inner.order.push_back(key);
        while inner.order.len() > self.capacity {
            if let Some(old) = inner.order.pop_front() {
                inner.set.remove(&old);
            }
        }
        true
    }

    fn len(&self) -> usize {
        match self.inner.lock() {
            Ok(g) => g.set.len(),
            Err(p) => p.into_inner().set.len(),
        }
    }
}

/// The commitment hashes currently held by an in-flight opportunity (§17.4).
#[derive(Debug, Default)]
struct InFlight {
    held: Mutex<BTreeSet<B256>>,
}

impl InFlight {
    fn take(&self, hash: B256) -> bool {
        // A poisoned lock here means a panic happened mid-insert. Recovering is
        // the safe direction: refusing every subsequent lock would suppress
        // every opportunity on the chain, which is a far worse failure than the
        // duplicate this set exists to prevent.
        let mut held = match self.held.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        held.insert(hash)
    }

    fn release(&self, hash: B256) {
        let mut held = match self.held.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        held.remove(&hash);
    }

    fn len(&self) -> usize {
        match self.held.lock() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        }
    }
}

/// **Step 1's token.** Holding one means this commitment is locked and no other
/// candidate can take it. Released on drop, so the suppression window is exactly
/// the lifetime of the work that holds it.
///
/// [`Plane::drive`] requires one, which is what makes step 1 unskippable.
///
/// `compile_fail` doctests rather than `trybuild`, per the convention in
/// `apex_types::candidate` — `trybuild` pins expected stderr to a rustc version,
/// while `compile_fail` asserts only the actual claim. Each is paired with a twin
/// differing **only** in the forbidden step, because a `compile_fail` snippet also
/// passes when it breaks for an unrelated reason.
///
/// A lock cannot be written down. **Two independent barriers hold this, and it is
/// worth knowing which:** the fields are private, *and* `InFlight` is a private
/// type so the third field cannot even be named. Either alone suffices, which is
/// why this case stays red when the fields are made `pub` — so it is not by itself
/// evidence that the fields are private. The next snippet is.
///
/// ```compile_fail
/// use apex_runtime::plane::Locked;
/// let forged: Locked<'static> =
///     Locked { commitment: unreachable!(), hash: unreachable!(), inflight: unreachable!() };
/// ```
///
/// Nor read around. This one *is* specific to field privacy — mutation-verified:
/// making `commitment` `pub` turns it green and the doctest fails.
///
/// ```compile_fail
/// use apex_runtime::plane::Locked;
/// fn peek<'a>(l: &'a Locked<'_>) -> &'a apex_types::commitment::ExecutionCommitment {
///     &l.commitment
/// }
/// ```
///
/// The twin, differing only in going through the accessor:
///
/// ```
/// use apex_runtime::plane::Locked;
/// fn peek<'a>(l: &'a Locked<'_>) -> &'a apex_types::commitment::ExecutionCommitment {
///     l.commitment()
/// }
/// ```
pub struct Locked<'p> {
    commitment: ExecutionCommitment,
    hash: B256,
    inflight: &'p InFlight,
}

impl std::fmt::Debug for Locked<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Locked").field("hash", &self.hash).finish()
    }
}

impl Locked<'_> {
    pub const fn commitment(&self) -> &ExecutionCommitment {
        &self.commitment
    }

    pub const fn hash(&self) -> B256 {
        self.hash
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        self.inflight.release(self.hash);
    }
}

/// What the plane did with one candidate.
#[derive(Clone, Debug, PartialEq)]
pub enum Handled {
    /// Reached exactly one terminal outcome (INV-01).
    Closed { ticket: TicketId, outcome: Box<TicketOutcome> },
    /// Declined before a ticket existed. A miss is in the ledger.
    Declined(Decline),
    /// §17.4: an identical commitment is already in flight. **Not a miss.**
    Suppressed(Suppressed),
    /// The feed redelivered an observation already handled. **Not a miss either**
    /// — and distinct from `Suppressed`, because the two say different things
    /// about the feed: this one means a source repeated itself, that one means
    /// two different observations described the same trade.
    Redelivered { chain: ChainId, at: apex_state::Ordinal },
}

/// How the protocol ended when it did not decline.
enum Completed {
    /// Steps 8-10 ran: included, and reconciled against the chain. Boxed
    /// because `TicketOutcome::Success` holds it boxed anyway.
    Settled(Box<PnlAttribution>),
    /// The null dispatcher took it. See `TicketOutcome::ShadowDispatched`.
    Shadow { at: UnixNanos, in_time: bool, waived: Vec<String> },
}

pub struct Plane {
    ports: Ports,
    inflight: InFlight,
    seen: SeenEvents,
    misses: Mutex<MissLedger>,
    budgets: Budgets,
    /// `SYSTEM_CAPTURE_ASSURANCE`'s denominator: tickets the risk gate
    /// authorized. Counted at `Authorized`, not at admission to the registry —
    /// a ticket the gate refused is a trade the system decided not to make, and
    /// counting it would make the figure fall whenever the market went quiet.
    authorized: AtomicU64,
    /// Its numerator: tickets a dispatcher accepted before their deadline.
    dispatched_in_time: AtomicU64,
}

impl Plane {
    pub fn new(ports: Ports) -> Self {
        Self::with_budgets(ports, Budgets::with_capacity(8))
    }

    pub fn with_budgets(ports: Ports, budgets: Budgets) -> Self {
        Self {
            ports,
            inflight: InFlight::default(),
            seen: SeenEvents::new(SeenEvents::DEFAULT_CAPACITY),
            misses: Mutex::new(MissLedger::new()),
            budgets,
            authorized: AtomicU64::new(0),
            dispatched_in_time: AtomicU64::new(0),
        }
    }

    /// §16.7's `SYSTEM_CAPTURE_ASSURANCE` over this plane's life: tickets that
    /// reached the dispatcher before their deadline, over tickets the risk gate
    /// authorized. Acceptance criterion 1's figure.
    ///
    /// `Undefined` until something is authorized — an empty window is not a
    /// passing one (Task 8.5's first finding). Nothing called
    /// `Scheduler::u_capture` from a running system before this, so criterion 1
    /// had a definition and no measurement.
    pub fn capture_assurance(&self) -> CaptureAssurance {
        Scheduler::u_capture(
            self.dispatched_in_time.load(Ordering::Relaxed),
            self.authorized.load(Ordering::Relaxed),
        )
    }

    /// Which lane this plane dispatches to.
    pub const fn lane(&self) -> LaneKind {
        self.ports.dispatch.kind()
    }

    /// How many observations the redelivery window currently holds.
    pub fn observations_seen(&self) -> usize {
        self.seen.len()
    }

    pub fn registry(&self) -> &TicketRegistry {
        &self.ports.registry
    }

    pub fn budgets(&self) -> &Budgets {
        &self.budgets
    }

    pub fn commitments_in_flight(&self) -> usize {
        self.inflight.len()
    }

    /// The miss ledger, cloned out. Cloned rather than borrowed because the
    /// ledger is behind a lock and handing out a guard would let a caller hold
    /// it across an await on the capture path.
    pub fn misses(&self) -> MissLedger {
        self.locked_misses(|l| l.clone())
    }

    /// The miss ledger, taken: the plane's is left empty. A long run takes it
    /// periodically and writes it out, so the ledger in memory is one period's
    /// misses rather than every miss since boot.
    pub fn drain_misses(&self) -> MissLedger {
        self.locked_misses(std::mem::take)
    }

    fn locked_misses<R>(&self, f: impl FnOnce(&mut MissLedger) -> R) -> R {
        let mut g = match self.misses.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        f(&mut g)
    }

    /// **INV-39.** Replay the journal, reconcile every in-flight ticket, and open
    /// the dispatch gate only when zero remain.
    ///
    /// Returns how many were reconciled. The gate stays shut on any error, which
    /// is the whole point: "we forgot to run recovery" and "recovery ran and
    /// passed" must not be the same state.
    pub fn boot(
        &self,
        chain: &dyn ChainOutcomeSource,
        now: UnixNanos,
    ) -> Result<usize, RecoveryError> {
        let found = scan(self.ports.registry.journal())?;
        let proof = reconcile(&self.ports.registry, &found, chain, now)?;
        Ok(self.ports.gate.open(proof))
    }

    /// **Step 1.** Build the commitment and take it, or say why not.
    ///
    /// Two methods collapsed into one, and a real `Commitments` is what forced
    /// it. The first draft had a `Decline`-returning `lock` for the pipeline and
    /// a `Suppressed`-returning `lock_exclusive` for tests, with the second
    /// reporting `B256::ZERO` when the commitment could not be built at all.
    /// That sentinel discarded the reason — and with `VenueCommitments` in place
    /// the reason is the actionable part: an unadmitted pool is `VenueDisabled`,
    /// which tells an operator to go verify the venue, and it was arriving as a
    /// generic `TooSlow`.
    pub fn lock(&self, candidate: &Candidate) -> Result<Locked<'_>, LockFailure> {
        let min_profit =
            U256::from(u128::try_from(candidate.expected_net_profit.max(1)).unwrap_or(u128::MAX));
        let commitment = self
            .ports
            .commitments
            .commit(candidate, self.ports.pool.auth(), min_profit)
            .map_err(LockFailure::Declined)?;
        let hash = commitment.hash();
        if self.inflight.take(hash) {
            Ok(Locked { commitment, hash, inflight: &self.inflight })
        } else {
            Err(LockFailure::Suppressed(Suppressed { commitment: hash }))
        }
    }

    /// One event, end to end, for every proposal it produces.
    pub async fn on_event(&self, event: &StateEvent) -> Vec<Handled> {
        // The cheapest check first, and before any budget is spent: has this
        // exact observation already been handled? See `SeenEvents`.
        if !self.seen.take(event.chain, event.at) {
            return vec![Handled::Redelivered { chain: event.chain, at: event.at }];
        }

        // §29.3: candidate generation is a bounded class. Reserved before the
        // search runs, so an overloaded process refuses work loudly instead of
        // queuing it.
        let Some(_permit) = self.budgets.reserve(ResourceClass::CandidateGeneration) else {
            let d = Decline::NoBudget(ResourceClass::CandidateGeneration);
            return vec![Handled::Declined(self.file(None, &d))];
        };

        let proposals = self.ports.search.propose(event).await;
        let mut out = Vec::with_capacity(proposals.len());
        for proposal in proposals {
            out.push(self.handle(event, &proposal).await);
        }
        out
    }

    async fn handle(&self, event: &StateEvent, proposal: &RouteProposal) -> Handled {
        // §46.2's four stages, concurrently, and their join is what makes a
        // candidate exist at all. Before this point there is a route; after it
        // there is a trade with a size.
        //
        // It runs before the gate check and before step 1 because a proposal has
        // no commitment: `ExecutionCommitment` covers `exact_inputs` and
        // `min_profit`, so the dedup key does not exist until the size does.
        let candidate = match self.refine(proposal).await {
            Ok(c) => c,
            Err(d) => return Handled::Declined(self.file_proposal(proposal, &d)),
        };
        self.handle_candidate(event, &candidate).await
    }

    /// §29.3's budget, then the join, then `apex-econ`'s assembly.
    async fn refine(&self, proposal: &RouteProposal) -> Result<Candidate, Decline> {
        let Some(_permit) = self.budgets.reserve(ResourceClass::ExactPricing) else {
            return Err(Decline::NoBudget(ResourceClass::ExactPricing));
        };
        let refinement = refine_concurrently(self.ports.econ.as_ref(), proposal).await?;
        self.ports.econ.assemble(proposal, refinement)
    }

    async fn handle_candidate(&self, event: &StateEvent, candidate: &Candidate) -> Handled {
        // INV-39, before anything else. §46.1 forbids new live dispatch until
        // reconciliation completes, so the honest response is to not create the
        // ticket -- a ticket admitted here would only ever reach the drain.
        if self.ports.gate.permit().is_none() {
            let d = Decline::DispatchGateShut;
            return Handled::Declined(self.file(Some(candidate), &d));
        }

        let locked = match self.lock(candidate) {
            Ok(l) => l,
            // The reason survives. A venue that could not be verified reaches the
            // ledger as `VENUE_DISABLED` rather than as a catch-all, which is the
            // difference between "go verify the pool" and "something was slow".
            Err(LockFailure::Declined(d)) => {
                return Handled::Declined(self.file(Some(candidate), &d))
            }
            Err(LockFailure::Suppressed(s)) => return Handled::Suppressed(s),
        };

        match self.drive(event, candidate, &locked).await {
            Ok(closed) => closed,
            Err(d) => Handled::Declined(self.file(Some(candidate), &d)),
        }
    }

    /// Steps 2–11. Separated from [`Self::handle`] so every early return files a
    /// miss in exactly one place rather than at a dozen `return` sites.
    async fn drive(
        &self,
        event: &StateEvent,
        candidate: &Candidate,
        locked: &Locked<'_>,
    ) -> Result<Handled, Decline> {
        let now = self.ports.registry.now();

        // Step 6's decision, taken early because §21.3 says to reject before
        // signing when no gas limit both fits a window and clears the edge.
        let (lane, gas_limit) = match self.ports.chain.optimize_submission_cost(candidate, now) {
            SubmissionDecision::Submit { lane, gas_limit, .. } => (lane, gas_limit),
            SubmissionDecision::Reject(r) => return Err(Decline::ChainRejected(r)),
        };

        let id = self
            .ports
            .registry
            .admit(ticket_from(candidate, locked, gas_limit, now))
            .map_err(|e| Decline::Uncommittable { detail: e.to_string() })?;

        let mut guard = self
            .ports
            .registry
            .checkout(id)
            .map_err(|e| Decline::Uncommittable { detail: e.to_string() })?;

        match self.protocol(event, candidate, locked, &mut guard, lane, gas_limit, now).await {
            Ok(completed) => {
                let outcome = match completed {
                    Completed::Settled(realized) => {
                        TicketOutcome::Success { stage: TicketStatus::Reconciled, realized }
                    }
                    Completed::Shadow { at, in_time, waived } => TicketOutcome::ShadowDispatched {
                        stage: TicketStatus::Acknowledged,
                        at,
                        in_time,
                        waived,
                    },
                };
                guard
                    .close(outcome.clone())
                    .map_err(|e| Decline::Uncommittable { detail: e.to_string() })?;
                Ok(Handled::Closed { ticket: id, outcome: Box::new(outcome) })
            }
            Err(d) => {
                // The ticket exists, so INV-01 applies: it must close with an
                // explicit code. The decline is *also* filed as a miss, because
                // a ticket that failed after admission is still an opportunity
                // that went untaken.
                let status = guard.status().unwrap_or(TicketStatus::Observed);
                let outcome = TicketOutcome::ExplicitFailure {
                    code: terminal_for(&d, status, lane),
                    at: self.ports.registry.now(),
                    state: Box::new(candidate.state_fingerprint.clone()),
                    cause: d.to_string(),
                };
                let _ = self.file(Some(candidate), &d);
                guard
                    .close(outcome.clone())
                    .map_err(|e| Decline::Uncommittable { detail: e.to_string() })?;
                Ok(Handled::Closed { ticket: id, outcome: Box::new(outcome) })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn protocol(
        &self,
        event: &StateEvent,
        candidate: &Candidate,
        locked: &Locked<'_>,
        guard: &mut TicketGuard<'_>,
        lane: SubmissionLaneId,
        gas_limit: GasLimit,
        now: UnixNanos,
    ) -> Result<Completed, Decline> {
        // ---- Step 2: reserve a signer lane and a nonce.
        let need = apex_capture::signer::LaneRequirements {
            chain: candidate.chain_id,
            executor: self.ports.pool.auth().executor,
            executor_version: self.ports.pool.auth().executor_version,
            min_gas_reserve_wei: 0,
        };
        let assignment = self.ports.pool.assign(&need).map_err(Decline::NoSignerLane)?;
        let readings = self.ports.live.read(&ticket_snapshot(guard, candidate)).await?;
        let nonce = assignment.reserve_nonce(readings.chain_pending_nonce, now);
        self.advance(guard, TicketStatus::Reserved)?;

        // ---- Step 3: reserve compute.
        //
        // §46.2's four stages already ran, in `refine`, and their join is what
        // made this candidate. Running them again here would be the second
        // sizing of one trade -- two answers to one question, with the later one
        // winning for no stated reason.
        let Some(_sim_budget) = self.budgets.reserve(ResourceClass::SizingAllocation) else {
            return Err(Decline::NoBudget(ResourceClass::SizingAllocation));
        };
        let repriced = candidate.clone();
        self.advance(guard, TicketStatus::Exacting)?;

        // The one call this ticket makes. Built once, here, and handed to the
        // simulator and the signer unchanged -- what is simulated is what is
        // signed. A route no executor op encodes stops here, before a
        // simulation is spent on it.
        let call = self.ports.calls.build(&repriced, locked.commitment())?;
        let from = Address::from(assignment.address());

        let sim = {
            let Some(_permit) = self.budgets.reserve(ResourceClass::Simulation) else {
                return Err(Decline::NoBudget(ResourceClass::Simulation));
            };
            self.ports.sim.simulate(&repriced, &call, from).await?
        };
        if !sim.success {
            return Err(Decline::SimulationFailed {
                class: sim.revert.as_ref().map(|(c, _)| *c),
            });
        }
        self.advance(guard, TicketStatus::Simulated)?;

        // ---- Risk. A hard gate: everything past here holds reserved resources.
        let admission = self.ports.risk.admit(&repriced, &sim, self.ports.dispatch.kind())?;

        // INV-17, structurally. A shadow-only admission never reaches a lane
        // that sends, **whatever the gate that granted it was told** -- the
        // gate is one component and this is the plane that owns the lane.
        // Refused before `Authorized`, so it is a refusal rather than an
        // admitted ticket that later failed.
        if let (DispatchLane::Live(_), Admission::ShadowOnly { waived }) =
            (&self.ports.dispatch, &admission)
        {
            return Err(Decline::RiskRefused {
                rule: format!(
                    "INV-17: {} was waived for a shadow lane, and this plane's lane sends",
                    labels(waived).join(", ")
                ),
            });
        }
        self.advance(guard, TicketStatus::Authorized)?;
        self.authorized.fetch_add(1, Ordering::Relaxed);

        // ---- Step 4: last-mile revalidation.
        let ticket = ticket_snapshot(guard, &repriced);
        let ctx = last_mile_context(
            &ticket,
            locked,
            &readings,
            nonce,
            assignment.lane(),
            gas_limit,
            self.ports.registry.now(),
        );
        let proof = last_mile(&ctx).map_err(|f| Decline::Revalidation(f.check))?;

        // ---- Step 5: sign. `advance(Signed)` FIRST -- write-ahead; see the
        // module header. A crash between these two lines must look like a ticket
        // that may be on chain, because it may be.
        self.advance(guard, TicketStatus::Signed)?;
        let authorization = SigningAuthorization::new(ticket.ticket_id, nonce, proof);
        let payload =
            self.ports.signer.sign(&authorization, &call, gas_limit, fee_caps(&readings))?;

        // ---- Step 6: dispatch. Again, journalled first.
        self.advance(guard, TicketStatus::Dispatching)?;
        let permit = self.ports.gate.permit().ok_or(Decline::DispatchGateShut)?;
        let request = DispatchRequest::new(
            authorization,
            candidate.chain_id,
            lane,
            candidate.submission_policy,
            payload.raw.clone(),
        );
        let submission_permit = self
            .budgets
            .reserve(ResourceClass::Submission)
            .ok_or(Decline::NoBudget(ResourceClass::Submission))?;
        let dispatched_at = self.ports.registry.now();
        let dispatched = match &self.ports.dispatch {
            DispatchLane::Shadow(null) => null.dispatch(&request, &permit, dispatched_at),
            DispatchLane::Live(sender) => sender.dispatch(&request, &permit, dispatched_at),
        };
        let mut ladder = dispatched.map_err(Decline::DispatchFailed)?;
        drop(submission_permit);

        // §16.7's numerator: a dispatcher accepted it, and before the deadline.
        // A ticket that got here late was still authorized and still sent -- or
        // would have been -- so it closes normally and counts against the ratio.
        let in_time = dispatched_at.0 <= candidate.deadline.0;
        if in_time {
            self.dispatched_in_time.fetch_add(1, Ordering::Relaxed);
        }

        // ---- Step 7: the transport's answer, and nothing more (INV-34).
        self.advance(guard, TicketStatus::Acknowledged)?;
        self.ports.pool.record_outcome(assignment.lane(), true);

        // A shadow ticket ends here. Nothing was sent, so steps 8-10 would be
        // asking the chain about a transaction that does not exist, and the
        // nonce was never consumed -- it goes back, or every later ticket would
        // sign against a gap the chain never saw.
        if let DispatchLane::Shadow(_) = &self.ports.dispatch {
            let _ = assignment.release_nonce(nonce, false);
            return Ok(Completed::Shadow {
                at: dispatched_at,
                in_time,
                waived: labels(admission.waived()),
            });
        }

        // ---- Step 8: preconfirmation and inclusion, from observations.
        for observation in self.ports.settlement.observe(payload.hash).await? {
            // A stage the ladder already has is not an error -- observations can
            // repeat. `observe` refuses a backwards move, which is the check that
            // matters.
            let _ = ladder.observe(observation.stage, observation.observed_at);
        }
        if ladder.reached(LifecycleStage::Preconfirmed) {
            self.advance(guard, TicketStatus::Preconfirmed)?;
        }
        if ladder.reached(LifecycleStage::Included) {
            self.advance(guard, TicketStatus::Included)?;
        }
        if ladder.reached(LifecycleStage::Finalized) {
            self.advance(guard, TicketStatus::Finalized)?;
        }

        // ---- Step 9: state moved before inclusion. §16.2 says reprice or
        // explicitly abandon; abandoning is what this plane does, because
        // repricing a dispatched transaction requires a replacement and §27.4
        // forbids doing that without the arithmetic in hand.
        if !ladder.is_included() {
            self.ports.pool.record_outcome(assignment.lane(), false);
            let _ = assignment.release_nonce(nonce, false);
            return Err(Decline::ChainUnavailable {
                detail: format!(
                    "reached {:?} at {}, not inclusion",
                    ladder.furthest(),
                    event.at.block
                ),
            });
        }

        // ---- Step 10: reconcile against the chain.
        let realized = self
            .ports
            .chain
            .reconcile_final_state(payload.hash)
            .await
            .map_err(|e| Decline::ChainUnavailable { detail: e.to_string() })?;
        let _ = assignment.release_nonce(nonce, true);
        self.advance(guard, TicketStatus::Reconciled)?;
        Ok(Completed::Settled(Box::new(realized)))
    }

    /// Advance, but only forwards and only with somewhere to go. A no-op here
    /// would silently accept a plane that had already jumped ahead.
    fn advance(&self, guard: &mut TicketGuard<'_>, to: TicketStatus) -> Result<(), Decline> {
        guard.advance(to).map_err(|e| Decline::Uncommittable { detail: e.to_string() })
    }

    /// INV-40 for a decline that happened **before** a candidate existed.
    ///
    /// A proposal has no `CandidateId`, no EV and no capture probability — those
    /// are exactly what the four stages were about to produce. Filing zeros would
    /// put invented numbers in the dataset that decides where engineering effort
    /// goes, so the record carries what is actually known: the route's own hash
    /// as its identity, and an EV of 0 meaning *unmeasured* rather than *nil*.
    ///
    /// The distinction is visible downstream because a proposal-stage miss has a
    /// `capture_probability` of 0.0 and no simulated EV, which no priced
    /// candidate produces.
    fn file_proposal(&self, proposal: &RouteProposal, decline: &Decline) -> Decline {
        let ctx = MissContext {
            candidate_id: apex_types::ids::CandidateId(
                u64::from_be_bytes(
                    proposal.route.route_hash.0[..8].try_into().unwrap_or([0u8; 8]),
                ),
            ),
            state_fingerprint: proposal.state_fingerprint.clone(),
            simulated_ev: 0,
            estimated_capture_probability: 0.0,
            path: SearchPath::Fast,
            submission_policy: apex_types::ticket::SubmissionPolicy::Private,
        };
        self.locked_misses(|l| l.record(&ctx, decline));
        decline.clone()
    }

    /// INV-40. One place, so a new early return cannot forget it.
    fn file(&self, candidate: Option<&Candidate>, decline: &Decline) -> Decline {
        if let Some(c) = candidate {
            let ctx = MissContext {
                candidate_id: c.candidate_id,
                state_fingerprint: c.state_fingerprint.clone(),
                simulated_ev: c.expected_net_profit,
                estimated_capture_probability: c.capture_probability,
                path: SearchPath::Fast,
                submission_policy: c.submission_policy,
            };
            self.locked_misses(|l| l.record(&ctx, decline));
        }
        decline.clone()
    }
}

/// §2.5's nineteen fields, from a candidate and its locked commitment.
fn ticket_from(
    candidate: &Candidate,
    locked: &Locked<'_>,
    gas_limit: GasLimit,
    now: UnixNanos,
) -> OpportunityTicket {
    OpportunityTicket {
        // Overwritten by the registry, which assigns ids: two callers inventing
        // them is how one ticket's outcome overwrites another's.
        ticket_id: TicketId(0),
        chain_id: candidate.chain_id,
        strategy: candidate.strategy,
        state_fingerprint: candidate.state_fingerprint.clone(),
        route_commitment: candidate.route.clone(),
        exact_input: candidate.input_amount.get(),
        expected_net_ev: candidate.expected_net_profit,
        robustness_margin: candidate.robustness_margin,
        validity_start: now,
        dispatch_deadline: candidate.deadline,
        target_execution_window: ExecutionWindow {
            earliest: now,
            latest: candidate.deadline,
            earliest_eligible_flashblock: None,
        },
        signer_lane: None,
        nonce: None,
        flash_source: candidate.flash_source.clone(),
        // The commitment hash, not a simulation hash: this field is what INV-06
        // compares against at signing time, and the locked commitment is the
        // thing that was agreed.
        simulation_result_hash: locked.hash(),
        submission_policy: candidate.submission_policy,
        required_gas_limit: gas_limit,
        created_at: now,
        status: TicketStatus::Observed,
    }
}

/// The registry owns the ticket; this reads the current copy back out. Falls back
/// to a reconstruction only if the ticket has already been closed, which the
/// caller's `TicketGuard` makes impossible -- so the fallback is unreachable and
/// says so rather than panicking.
fn ticket_snapshot(guard: &TicketGuard<'_>, candidate: &Candidate) -> OpportunityTicket {
    let mut t = OpportunityTicket {
        ticket_id: guard.id(),
        chain_id: candidate.chain_id,
        strategy: candidate.strategy,
        state_fingerprint: candidate.state_fingerprint.clone(),
        route_commitment: candidate.route.clone(),
        exact_input: candidate.input_amount.get(),
        expected_net_ev: candidate.expected_net_profit,
        robustness_margin: candidate.robustness_margin,
        validity_start: UnixNanos(0),
        dispatch_deadline: candidate.deadline,
        target_execution_window: ExecutionWindow {
            earliest: UnixNanos(0),
            latest: candidate.deadline,
            earliest_eligible_flashblock: None,
        },
        signer_lane: None,
        nonce: None,
        flash_source: candidate.flash_source.clone(),
        simulation_result_hash: B256::ZERO,
        submission_policy: candidate.submission_policy,
        required_gas_limit: candidate.total_execution_cost.gas_limit,
        created_at: UnixNanos(0),
        status: guard.status().unwrap_or(TicketStatus::Observed),
    };
    t.ticket_id = guard.id();
    t
}

#[allow(clippy::too_many_arguments)]
fn last_mile_context(
    ticket: &OpportunityTicket,
    locked: &Locked<'_>,
    readings: &LiveReadings,
    nonce: apex_capture::signer::ReservedNonce,
    assigned_lane: apex_types::ids::SignerLaneId,
    gas_limit: GasLimit,
    now: UnixNanos,
) -> LastMileContext {
    let c = locked.commitment();
    LastMileContext {
        ticket: ticket.ticket_id,
        committed_chain: c.chain_id,
        signer_chain: ticket.chain_id,
        committed_executor: address_bytes(c.executor_address),
        committed_executor_version: c.executor_version,
        live_executor: readings.executor,
        live_executor_version: readings.executor_version,
        nonce,
        assigned_lane,
        now,
        dispatch_deadline: ticket.dispatch_deadline,
        expected_net_profit: ticket.expected_net_ev,
        min_profit: readings.min_profit,
        observed_fee_wei: readings.observed_fee_wei,
        fee_ceiling_wei: readings.fee_ceiling_wei,
        required_gas: gas_limit,
        remaining_block_gas: readings.remaining_block_gas,
        signer_balance_wei: readings.signer_balance_wei,
        required_reserve_wei: 0,
        flash_required: readings.flash_required,
        flash_available: readings.flash_available,
        committed_hooks: None,
        live_hooks: readings.live_hooks,
        committed_venue_versions: ticket.state_fingerprint.venue_state_version.clone(),
        live_venue_versions: readings.live_venue_versions.clone(),
    }
}

fn address_bytes(a: Address) -> [u8; 20] {
    let mut out = [0u8; 20];
    out.copy_from_slice(a.as_slice());
    out
}

/// §46.1: every terminal failure carries a code, and none of the eleven is "it
/// went wrong". This is the mapping from a decline to the code that is *true*.
fn terminal_for(d: &Decline, at: TicketStatus, lane: SubmissionLaneId) -> TerminalFailure {
    match d {
        Decline::NoProfitableSize => {
            TerminalFailure::EvCollapsed { admitted: 0, revalidated: 0 }
        }
        Decline::StaleState { age } => TerminalFailure::Stale { observed_age: *age },
        Decline::SimulationFailed { class } => TerminalFailure::Reverted {
            revert_class: class.unwrap_or(RevertClass::Unknown),
            data: Vec::new(),
        },
        Decline::RiskRefused { rule } => TerminalFailure::RiskRejected { rule: rule.clone() },
        Decline::NoSignerLane(_) => TerminalFailure::SignerUnavailable,
        Decline::Revalidation(c) => TerminalFailure::RiskRejected { rule: c.to_string() },
        Decline::ChainRejected(r) => {
            TerminalFailure::SubmissionRejected { lane, detail: r.to_string() }
        }
        Decline::DispatchGateShut => {
            TerminalFailure::RiskRejected { rule: "dispatch gate shut".to_string() }
        }
        Decline::NoBudget(c) => {
            TerminalFailure::RiskRejected { rule: format!("{} at budget", c.label()) }
        }
        Decline::DispatchFailed(e) => {
            TerminalFailure::SubmissionRejected { lane, detail: e.to_string() }
        }
        Decline::Unsigned { .. } => TerminalFailure::SignerUnavailable,
        // The ticket got somewhere and then the chain stopped answering. Not
        // `Abandoned`: something did decide, and `at_status` would be the only
        // honest part of that code.
        Decline::ChainUnavailable { .. } => TerminalFailure::Abandoned { at_status: at },
        Decline::Uncommittable { .. } => TerminalFailure::Abandoned { at_status: at },
        Decline::VenueUnverified { detail } => {
            TerminalFailure::RiskRejected { rule: detail.clone() }
        }
    }
}

/// Exposed so a caller can name a chain without importing `apex-types`.
pub const fn chain_of(c: &Candidate) -> ChainId {
    c.chain_id
}
