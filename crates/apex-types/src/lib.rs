//! APEX-MEV v4 shared vocabulary.
//!
//! Written first, and deliberately dependency-light, so every other crate
//! depends on a stable set of names rather than reaching into `main.rs` structs.
//! No I/O, no async, no provider types (PLAN.md §5.2, §7).
//!
//! # Primitives
//!
//! `alloy-primitives`, not `ethers` (PLAN.md §2.2 C-10). It is already in
//! `Cargo.lock` via `revm` 20, so this adds nothing to the dependency tree, and
//! Phase 4's `eth_simulateV1` backend needs it because `ethers` 2.0.14 cannot
//! type that call. `arb-exec-legacy` keeps `ethers` until Phase 17; the single
//! conversion boundary will live in `apex_types::compat`, added in Phase 1 when
//! the first crate actually has to cross it.
//!
//! # What the type system is carrying
//!
//! Several blueprint invariants are enforced here rather than by review, because
//! this repository has shipped the same class of bug four times -- something
//! written in one phase and never wired in the next (`break_continuity`, the
//! `metrics: None` construction, `anchor_cl`/`anchor_v2`, and the candidate-log
//! fields declared but never populated). A rule that lives only in prose is a
//! rule that gets forgotten.
//!
//! - [`ticket::TicketStatus`] is monotonic; `advance` is the only mutator.
//! - [`miss::MissReason`] is exhaustive with no catch-all.
//! - [`cost::GasLimit`] and [`cost::GasUsed`] have no conversion between them.
//! - [`candidate::DiscreteSize`] cannot be minted from a continuous optimum.
//! - [`state::ReconstructionStatus`] has no `Default`.
//! - [`state::StateFingerprint`] has no `Default`.

pub mod ack;
pub mod candidate;
pub mod commitment;
pub mod compat;
pub mod cost;
pub mod flash;
pub mod ids;
pub mod miss;
pub mod pnl;
pub mod risk;
pub mod route;
pub mod sim;
pub mod state;
pub mod ticket;
pub mod time;

/// Crate version, exposed so workspace wiring is testable before any real type
/// is consumed.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
