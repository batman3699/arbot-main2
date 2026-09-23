//! `ChainExecutionAdapter` (§20, Blueprint §4).
//!
//! **A universal interface with chain-specific economics.** Every
//! chain-conditional branch currently living in `main.rs` —
//! `chain_hot_pool_base_cap(&str)`, `derive_chain_event_sampling_rate(&str)`,
//! `derive_chain_time_budget_ms` — becomes an adapter method or an
//! adapter-owned value. `scripts/ci/no_chain_string_matching.sh` fails the
//! build on `match chain_name` or `chain == "base"` outside this crate.
//!
//! # Eleven methods, not ten
//!
//! §20's code block defines eleven; three places in the plan's prose say ten
//! (§3.4's file table, §7's crate table, BP-030). The code block is the
//! specification and the count in the prose is wrong — recorded rather than
//! quietly reconciled, because a trait whose method count nobody agrees on is
//! a trait somebody will implement partially.
//!
//! # No default implementations
//!
//! Same rule as `VenueAdapter` (§8.3, Task 2.3): a default is a chain silently
//! inheriting another chain's economics. A new chain must answer all eleven
//! questions or fail to compile.

use crate::regime::ChainRegime;
use apex_types::ack::LifecycleStage;
use apex_types::candidate::Candidate;
use apex_types::cost::{GasLimit, TotalExecutionCost};
use apex_types::ids::{ChainId, SubmissionLaneId};
use apex_types::miss::ObservedOutcome;
use apex_types::pnl::PnlAttribution;
use apex_types::sim::SimulationResult;
use apex_types::state::StateFingerprint;
use apex_types::time::{DurationNanos, UnixNanos};
use alloy_primitives::B256;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A live subscription to the chain's state feed. Opaque: what is behind it is
/// the adapter's business, and a caller that could inspect it would start
/// depending on one chain's feed shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateFeedHandle {
    pub chain: ChainId,
    pub feed: &'static str,
    pub opened_at: UnixNanos,
}

/// The exact bytes a signer produced, and what they are for.
///
/// INV-10: transport redundancy sends **these bytes**. A `SignedPayload` is
/// therefore not `Clone`-and-modify — there is no builder here, and a
/// replacement is a new payload with the same nonce, made deliberately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedPayload {
    pub chain: ChainId,
    pub hash: B256,
    pub nonce: u64,
    pub gas_limit: GasLimit,
    pub raw: Vec<u8>,
}

/// What a transport said when handed a payload. **Not inclusion** — an `Ack`
/// carries the stage it actually reached (INV-34), and the null dispatcher's
/// lesson applies to every real one: a transport that answers `Ok` has
/// answered `Ok`, and nothing more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ack {
    pub lane: SubmissionLaneId,
    pub stage: LifecycleStage,
    pub at: UnixNanos,
    /// The chain's own identifier for the submission, when it gave one.
    pub tx_hash: Option<B256>,
}

/// §4.1's submission decision.
#[derive(Clone, Debug, PartialEq)]
pub enum SubmissionDecision {
    Submit {
        lane: SubmissionLaneId,
        gas_limit: GasLimit,
        /// §22.2. `None` on chains without preconfirmation windows.
        earliest_eligible_flashblock: Option<u32>,
        max_fee_per_gas_wei: u128,
        max_priority_fee_per_gas_wei: u128,
    },
    /// §21.3: "if none exists → REJECT THE TICKET BEFORE SIGNING". The reason
    /// is carried because a rejection nobody can explain is a capture miss
    /// nobody can fix.
    Reject(RejectReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// §21.3: no gas limit is both safe and fits an economically valuable
    /// window.
    NoSafeGasLimit { needed: GasLimit, largest_window: u64 },
    /// The state this candidate priced against will not survive to the earliest
    /// window it could land in.
    StateExpiresFirst { valid_for: DurationNanos, earliest_landing: DurationNanos },
    /// The extra gas this adapter had to reserve costs more than the trade
    /// makes. This is the adapter's **own** contribution to the cost, not a
    /// re-check of `apex-econ`'s arithmetic: the candidate was priced against
    /// one gas limit and §21.3 raised it to cover the tail, so the difference
    /// is a cost nothing upstream knew about.
    GasHeadroomExceedsEdge { extra_wei: u128, edge_wei: u128 },
    /// No lane is configured for the policy this candidate requires.
    NoLaneForPolicy,
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSafeGasLimit { needed, largest_window } => write!(
                f,
                "needs {} gas; the largest eligible window holds {}",
                needed.0, largest_window
            ),
            Self::StateExpiresFirst { valid_for, earliest_landing } => write!(
                f,
                "state is valid for {} ns but the earliest landing is {} ns away",
                valid_for.0, earliest_landing.0
            ),
            Self::GasHeadroomExceedsEdge { extra_wei, edge_wei } => {
                write!(f, "the gas headroom costs {extra_wei}, above the edge {edge_wei}")
            }
            Self::NoLaneForPolicy => f.write_str("no lane is configured for the required policy"),
        }
    }
}

/// §27.4. **No blind gas escalation**: the policy answers with the current EV
/// and the incremental cost in hand, and defaults to refusing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplacementPolicy {
    pub supported: bool,
    /// What the chain requires of a replacement, if it takes them at all.
    pub min_fee_bump_bps: u32,
    /// A replacement may not be attempted more than this many times for one
    /// nonce, whatever the arithmetic says.
    pub max_attempts: u32,
}

impl ReplacementPolicy {
    /// §27.4: "replacement is allowed only while the opportunity's remaining EV
    /// exceeds the incremental replacement cost."
    ///
    /// Returns `false` by default and there is no unconditional path to `true`.
    pub const fn should_replace(
        &self,
        remaining_ev_wei: i128,
        incremental_cost_wei: i128,
        attempts_so_far: u32,
    ) -> bool {
        if !self.supported || attempts_so_far >= self.max_attempts {
            return false;
        }
        remaining_ev_wei > incremental_cost_wei
    }
}

/// What the adapter could not do. Distinguished so a caller can tell a chain
/// problem from a candidate problem — the first is a posture change, the second
/// is a miss.
#[derive(Clone, Debug, PartialEq)]
pub enum AdapterError {
    Unreachable { detail: String },
    NotSupportedOnThisChain { what: &'static str },
    Rejected(RejectReason),
    /// The chain answered something this adapter cannot interpret. Never
    /// converted into a default (§5.6).
    Uninterpretable { detail: String },
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable { detail } => write!(f, "chain unreachable: {detail}"),
            Self::NotSupportedOnThisChain { what } => write!(f, "{what} is not supported here"),
            Self::Rejected(r) => write!(f, "{r}"),
            Self::Uninterpretable { detail } => write!(f, "uninterpretable answer: {detail}"),
        }
    }
}

impl std::error::Error for AdapterError {}

pub type AdapterResult<T> = Result<T, AdapterError>;

/// What a chain reported about a transaction it already has. The verified
/// counterpart of `pending_state`.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingState {
    pub fingerprint: StateFingerprint,
    pub observed_at: UnixNanos,
    /// Base: the index within the current block. `None` where there are no
    /// preconfirmation windows.
    pub flashblock_index: Option<u32>,
}

/// §20's eleven methods.
#[async_trait]
pub trait ChainExecutionAdapter: Send + Sync {
    fn chain_id(&self) -> ChainId;

    /// The regime this adapter discovered. §20.1: a chain whose regime cannot
    /// be discovered is not admitted to live trading, so this returns a
    /// `Result` rather than a field.
    fn regime(&self, now: UnixNanos) -> AdapterResult<ChainRegime>;

    async fn state_feed(&self) -> AdapterResult<StateFeedHandle>;

    async fn pending_state(&self) -> AdapterResult<PendingState>;

    async fn simulate(&self, payload: &SignedPayload) -> AdapterResult<SimulationResult>;

    fn estimate_total_fee(&self, c: &Candidate) -> AdapterResult<TotalExecutionCost>;

    /// The probability this lands, **at a stated time**. Time is an argument
    /// because the answer decays: the same candidate is worth less the later
    /// it is asked about, and a method without it would be quietly reporting
    /// the value at whatever moment it happened to run.
    fn estimate_inclusion_probability(&self, c: &Candidate, at: UnixNanos) -> f64;

    /// §4.1's submission decision.
    ///
    /// **Takes `now`, which §20's signature does not.** §21.2's own decision
    /// flow ends "→ state validity at that time → submit / reject", and there
    /// is no way to evaluate *at that time* without one. The alternative was to
    /// drop the state-validity clause from this method, which would move the
    /// check to last-mile revalidation and leave the scheduler choosing windows
    /// a candidate cannot survive to — a signature spent on a dead trade.
    fn optimize_submission_cost(&self, c: &Candidate, now: UnixNanos) -> SubmissionDecision;

    async fn submit(&self, signed: &SignedPayload, lane: SubmissionLaneId) -> AdapterResult<Ack>;

    fn replacement_policy(&self) -> ReplacementPolicy;

    async fn observe_outcome(&self, h: B256) -> AdapterResult<ObservedOutcome>;

    async fn reconcile_final_state(&self, h: B256) -> AdapterResult<PnlAttribution>;
}

/// INV-40. A submission decision that declines still has to say which bucket
/// the lost opportunity belongs in.
///
/// `GasHeadroomExceedsEdge` is `GasFail` rather than `LowEv`, and the
/// distinction matters to the ledger: the candidate DID clear its costs as
/// priced, and what killed it was §21.3's headroom over the p99. A spike in
/// this bucket says the gas model's tail is too fat, not that the market went
/// quiet.
impl apex_types::miss::ExplainsMiss for RejectReason {
    fn miss_reason(&self) -> apex_types::miss::MissReason {
        use apex_types::miss::MissReason as R;
        match self {
            Self::NoSafeGasLimit { .. } => R::EarliestFlashblockTooLate,
            Self::StateExpiresFirst { .. } => R::StaleState,
            Self::GasHeadroomExceedsEdge { .. } => R::GasFail,
            Self::NoLaneForPolicy => R::RiskFail,
        }
    }
}
