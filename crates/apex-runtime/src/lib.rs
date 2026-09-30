//! The §46 deterministic control plane.
//!
//! # What this crate replaces, and what the replacement has to prove
//!
//! §4.4 records the defect: `Runner` / `RunnerConfig` in `src/main.rs`, ~120
//! fields, owning "everything: scan loop, candidate prep, pricing, sizing, gas,
//! flash, simulation, risk, dispatch, accounting" — **"working, untestable as
//! components"**. Audit P2-7 flagged it at 13,039 lines and it reached 16,659
//! *while flagged*, which is the argument for rebuilding rather than deferring
//! again.
//!
//! The failure was never the line count. A 16,000-line file assembled from
//! independently testable parts would be ugly; this one could not be driven at
//! all without a node, a key and a network. So the replacement's specification
//! is its test: **the whole ticket lifecycle runs from a recorded event stream
//! with no socket open.** `tests/end_to_end.rs` is that claim, and it is the
//! reason every external effect here sits behind a port.
//!
//! # Four things assembled, one thing added
//!
//! | Module | §  | What it owns |
//! |---|---|---|
//! | [`bus`] | §2.6, §46.3 | The event feed, and the rule that the slow path never delays the fast one |
//! | [`workers`] | §29, §46.2 | Nine resource classes with independent budgets, and the concurrent join |
//! | [`plane`] | §16.2 | The eleven-step capture protocol, assembled over ports |
//! | [`supervise`] | — | The legacy `spawn_supervised`, migrated and made stoppable |
//! | [`shutdown`] | §46.1 | A drain that refuses to classify what it cannot observe |
//!
//! The added thing is §17.4's duplicate suppression, which §7's crate table
//! assigned to `apex-capture` and Phase 6 did not build. It lands here because
//! its key is the `ExecutionCommitment` hash and the registry does not — and
//! should not — know what a commitment is: the registry accounts for tickets,
//! and suppression decides whether a ticket should exist at all. That decision
//! is step 1 of §16.2, and step 1 is here.

pub mod bus;
pub mod commit;
pub mod econ;
pub mod plane;
pub mod risk;
pub mod search;
pub mod shutdown;
pub mod supervise;
pub mod workers;

/// Crate version, exposed so workspace wiring is testable.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
