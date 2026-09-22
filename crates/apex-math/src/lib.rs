//! APEX-MEV v4 exact pricing mathematics.
//!
//! Everything in this crate is **pure**: no provider, no `async`, no clock, no
//! environment. Given the same inputs it returns the same answer, on any
//! machine, at any time. That is what makes the differential harness (§35.1)
//! meaningful — a divergence against the chain is a divergence in the maths,
//! never in what was fetched.
//!
//! The boundary is enforced by the dependency list, not by review: this crate
//! depends on `ethers-core`, so `Provider`, `Middleware` and `abigen!` are not
//! in scope here at all.
//!
//! # What lives here, and what does not
//!
//! Phase 2's scope correction (PLAN.md §33) split the legacy pricing modules on
//! **purity**, not on venue family. The nine modules that carry `abigen!`
//! bindings or `async fn` — `cl_sim`'s loader, `cl_ticks`, `quote_cl`,
//! `quote_univ2/3`, `quote_slipstream`, `quote_curve`, `quote_balancer`,
//! `discovery` — are RPC clients and belong in `apex-venues`. Two of them,
//! `quote_curve` and `quote_balancer`, are *only* RPC clients: there is no
//! local Curve or Balancer implementation in this repository yet, which is why
//! §4.7's `UNKNOWN` entries for their "exactness" were mis-stated.
//!
//! `quote_common::is_block_out_of_range_error` and `is_execution_revert`
//! classify JSON-RPC error strings. They are pure, so they compile here, but
//! they are RPC semantics and move to `apex-venues` with the transport layer.
//!
//! `cl_parity_gate` is pure of I/O but reads four environment variables, and
//! this crate must not read the environment. It arrives with Task 2.6, which
//! is where that env plane is dismantled.

pub mod cl_math;
pub mod cl_state;
pub mod cl_swap;
pub mod engine;
pub mod math;
pub mod quote_common;
pub mod quote_solidly;

/// Crate version, exposed so workspace wiring is testable independently of any
/// particular engine.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
