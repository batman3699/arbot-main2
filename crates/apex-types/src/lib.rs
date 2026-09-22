//! APEX-MEV v4 shared vocabulary.
//!
//! Written first, and deliberately dependency-free, so every other crate depends
//! on a stable set of names rather than on structs reached out of `main.rs`. No
//! I/O, no async, no provider types (PLAN.md §5.2, §7).
//!
//! Primitives will be `alloy-primitives`, not `ethers` -- see PLAN.md §2.2 C-10.
//! `alloy-primitives` is already in `Cargo.lock` via `revm` 20, so this costs no
//! new dependency, and Phase 4's `eth_simulateV1` backend needs it because
//! `ethers` 2.0.14 cannot type that call. `arb-exec-legacy` keeps `ethers` until
//! Phase 17; the single conversion boundary will live in `apex_types::compat`.

/// Crate version, exposed so the workspace wiring is testable before any real
/// type exists.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
