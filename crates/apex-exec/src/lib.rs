//! Transaction construction (Blueprint §25, §26.2).
//!
//! # The commitment this crate builds is the one the contract recomputes
//!
//! §25 specifies `apex_types::commitment::ExecutionCommitment`, whose `hash()`
//! covers a domain separator, the executor address and version, venue
//! fingerprints, a state-fingerprint hash, a route hash, exact inputs, min
//! profit, slippage constraints, a deadline and a submission policy.
//!
//! **The deployed executor recomputes something different.**
//! `MultiVenueArbImplementation.planCommitment(PlanV2)` hashes
//! `block.chainid`, `address(this)`, a plan version, a rolling hash over the
//! loans, `cycleSlippageBps`, a rolling hash over the steps, `minProfit`,
//! `declaredResidue`, `chainId` and `deadline` — and reverts with
//! `CommitmentMismatch` when the plan's declared value does not match.
//!
//! The two overlap and are not the same. A `Commitments` implementation built
//! against §25's Rust type would produce a value every live transaction rejects,
//! so [`commitment::plan_commitment`] mirrors the **contract**, and
//! `tests/plan_commitment.rs` holds it to a fixture the contract itself produced.
//! `ExecutionCommitment` keeps its place as the off-chain ticket-level record —
//! it carries things the chain has no way to check, such as the state
//! fingerprint the trade was priced against — and the divergence is recorded in
//! PLAN.md rather than reconciled by changing a reviewed contract mid-phase.
//!
//! # What the on-chain check does and does not catch
//!
//! Quoting the contract, because it is easy to claim more: *"the plan and its
//! commitment arrive in the same calldata from the same caller, so an executor
//! that wanted to run a different trade could simply commit to the different
//! trade. This check does not constrain a malicious executor."* What it
//! constrains is everything between the planner and the chain — an encoder that
//! builds a plan the planner did not describe, a field dropped by an ABI change,
//! a transport that corrupts a word. The off-chain half of INV-06 is the signer
//! refusing a payload whose recomputed commitment differs from the ticket's, and
//! the two halves catch different things.

pub mod call;
pub mod commitment;
pub mod encode;
pub mod sign;

pub use call::ExecutorCall;
pub use commitment::{plan_commitment, Loan, LoanProvider, Op, PlanV2, Step, PLAN_VERSION_V2};
pub use encode::{start_v2_calldata, EncodeError, PlanEncoder, START_V2_SELECTOR};
pub use sign::{CommitmentMismatch, SignedPlan};

/// Crate version, exposed so workspace wiring is testable.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
