//! APEX-MEV v4 economics.
//!
//! The question this crate answers is not "is there an arbitrage" — that is
//! `apex-search`'s — but **"how much, and does it clear what it costs"**.
//!
//! # Why sizing is a type, not a number
//!
//! §14.3 requires the executed size to be an integer candidate verified by
//! exact AMM evaluation, and forbids a continuous optimum becoming an execution
//! dependency. [`sizing::discrete::refine`] is the only function in the
//! workspace that can mint an `apex_types::DiscreteSize`, because it is the
//! only one that can produce the `DiscreteRefined` witness the constructor
//! demands. A continuous result cannot be assigned to `Candidate::input_amount`
//! — not by convention, by type.

pub mod cost;
pub mod sizing;

/// Crate version, exposed so workspace wiring is testable before any sizing
/// runs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
