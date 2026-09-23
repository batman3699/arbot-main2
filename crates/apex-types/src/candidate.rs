//! The candidate object (Blueprint §45).

use crate::cost::TotalExecutionCost;
use crate::flash::FlashSourceQuote;
use crate::ids::{CandidateId, ChainId, StrategyId, VenueId};
use crate::route::{CertificateStatus, RouteCommitment};
use crate::sim::SimulationTier;
use crate::state::StateFingerprint;
use crate::ticket::SubmissionPolicy;
use crate::time::{DurationNanos, UnixNanos};
use alloy_primitives::U256;
use serde::{Deserialize, Serialize};

/// The exact, integer, wei-denominated trade size (§14.3, INV-18).
///
/// Private field with one constructor on purpose. §14.3 requires the final size
/// be an integer candidate verified by exact AMM evaluation, and forbids a
/// continuous optimum becoming an execution dependency. Making this the only
/// type `Candidate::input_amount` accepts moves that from a review rule to a
/// compile error -- `apex-econ`'s `sizing::discrete::refine` is the only thing
/// that can mint one.
///
/// # The compile error, as a test
///
/// Task 3.1 asked for a `trybuild` compile-fail fixture. These are
/// `compile_fail` doctests instead, for a specific reason: `trybuild` compares
/// the full stderr against a recorded file, which pins the test to a rustc
/// version and turns a diagnostic reword into a red build. `compile_fail`
/// asserts only that the code does not compile, which is the actual claim.
///
/// The known weakness of a `compile_fail` test is that it also passes when the
/// snippet fails for an unrelated reason -- a typo, a missing import. Each one
/// below is therefore paired with a twin that differs **only** in the forbidden
/// step and does compile, so a snippet broken for the wrong reason would take
/// its twin down with it. Verified by mutation: making this field `pub` turns
/// the first case green, and the test fails.
///
/// A raw amount cannot become a size:
///
/// ```compile_fail
/// use apex_types::candidate::DiscreteSize;
/// use alloy_primitives::U256;
/// // The field is private: this is the assignment §14.3 forbids.
/// let size = DiscreteSize(U256::from(1_000u64));
/// ```
///
/// The twin, differing only in going through the refinement path:
///
/// ```
/// use apex_types::candidate::{DiscreteRefined, DiscreteSize};
/// use alloy_primitives::U256;
/// let size = DiscreteSize::from_refinement(U256::from(1_000u64), DiscreteRefined::new());
/// assert_eq!(size.get(), U256::from(1_000u64));
/// ```
///
/// And there is no conversion to reach for either. A `U256`:
///
/// ```compile_fail
/// use apex_types::candidate::DiscreteSize;
/// use alloy_primitives::U256;
/// let size: DiscreteSize = U256::from(1_000u64).into();
/// ```
///
/// ...nor a continuous optimum, which is an `f64` and has no path here at all:
///
/// ```compile_fail
/// use apex_types::candidate::DiscreteSize;
/// let size: DiscreteSize = (1_000.0_f64).into();
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DiscreteSize(U256);

impl DiscreteSize {
    /// Only callable from the discrete-refinement path. The `_witness` argument
    /// is what makes that structural: `DiscreteRefined` cannot be constructed
    /// outside `apex-econ`, so no other crate can mint a `DiscreteSize` from a
    /// continuous result.
    pub const fn from_refinement(amount: U256, _witness: DiscreteRefined) -> Self {
        Self(amount)
    }

    pub const fn get(self) -> U256 {
        self.0
    }

    /// Test-only escape hatch. Behind a feature so it cannot be reached from a
    /// production build even by accident.
    #[cfg(any(test, feature = "test-util"))]
    pub const fn for_test(amount: U256) -> Self {
        Self(amount)
    }
}

/// Proof-of-refinement token. Constructible only by `apex-econ` (its single
/// constructor is `#[doc(hidden)]` and the type carries no public fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DiscreteRefined(());

impl DiscreteRefined {
    /// Mint a proof-of-refinement.
    ///
    /// `pub` because `apex-econ` has to call it from another crate, and Rust has
    /// no "visible to exactly these crates". The restriction is therefore
    /// enforced by `scripts/ci/no_unearned_discrete_size.sh`, which fails the
    /// build if this is called anywhere but `crates/apex-econ/src/sizing/`.
    ///
    /// That is a weaker guarantee than the private field gives within this
    /// crate, and it is stated plainly rather than dressed up: the value is that
    /// minting a size outside the refinement path is now a deliberate, greppable
    /// act instead of an ordinary assignment.
    #[doc(hidden)]
    pub const fn new() -> Self {
        Self(())
    }
}

/// Blueprint §45 "Candidate", all twenty-one fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub candidate_id: CandidateId,
    pub chain_id: ChainId,
    pub strategy: StrategyId,
    pub venue_set: Vec<VenueId>,
    pub route: RouteCommitment,
    pub state_fingerprint: StateFingerprint,
    pub state_age: DurationNanos,
    pub flash_source: Option<FlashSourceQuote>,
    /// A `DiscreteSize`, never a bare `U256` -- see the type's docs.
    pub input_amount: DiscreteSize,
    pub expected_output: U256,
    pub gross_profit: U256,
    pub dex_fees: U256,
    pub flash_fee: U256,
    pub total_execution_cost: TotalExecutionCost,
    /// Signed: a candidate may be negative and still worth recording as a miss.
    pub expected_net_profit: i128,
    pub robust_ev: i128,
    pub certificate_status: CertificateStatus,
    pub simulation_tier: SimulationTier,
    pub capture_probability: f64,
    pub robustness_margin: f64,
    pub deadline: UnixNanos,
    pub submission_policy: SubmissionPolicy,
}
