//! The live canary (Task 8.5 R28): the reactive lane's first real trades, one
//! at a time, with every loss bounded and every trade reconciled to the wei.
//!
//! | Module | Does |
//! |---|---|
//! | `ledger` | `canary.jsonl`: every send written and synced before it leaves, every outcome after |
//! | `limits` | the running totals, the halts, the stop file: [`limits::Canary`], the risk gate's [`crate::risk::Hold`] |
//! | `send` | [`send::CanaryDispatcher`]: the plane's `Sender` on the canary lane |
//! | `chain` | [`chain::CanaryChain`]: receipts, settlement and reconciliation |
//! | `recover` | [`recover::CanaryRecovery`]: what a crash left open, resolved at boot |
//!
//! The spec is `docs/superpowers/specs/2026-10-15-live-canary-design.md`.

pub mod chain;
pub mod ledger;
pub mod limits;
pub mod recover;
pub mod send;

pub use ledger::{Halt, Outcome, Record, Sent, Settled};
pub use limits::{Canary, CanaryReport, Limits, Totals};
