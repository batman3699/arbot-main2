//! APEX-MEV v4 venue adapters.
//!
//! Everything here talks to a node. That is the line between this crate and
//! `apex-math`: `apex-math` depends on `ethers-core` and cannot name a
//! provider; this crate depends on the full `ethers` and does almost nothing
//! else. A module belongs here if it has an `abigen!` block, a `Provider`, or
//! an `async fn` — and if it does not, it belongs over there.
//!
//! The dependency runs one way, `apex-venues -> apex-math`, and Phase 2's
//! scope correction (PLAN.md §33) exists because the originally-specified file
//! allocation ran it both ways at once.
//!
//! # Not yet here
//!
//! `cl_parity_gate` is pure of I/O but reads four environment variables, so it
//! sits in neither crate cleanly and stays in `arb-exec` until Task 2.6
//! replaces that env plane with injected configuration. The same is true of
//! the five CL switches still in `arb_exec::cl_sim`.

pub mod adapter;
pub mod admission;
pub mod breaker;
pub mod cl_load;
pub mod cl_ticks;
pub mod discovery;
pub mod path;
pub mod quote_balancer;
pub mod quote_cl;
pub mod quote_curve;
pub mod quote_slipstream;
pub mod quote_univ2;
pub mod quote_univ3;
pub mod revert;

/// Crate version, exposed so workspace wiring is testable before any adapter
/// is consumed.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
