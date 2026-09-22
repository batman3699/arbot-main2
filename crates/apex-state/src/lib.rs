//! Canonical and speculative state (Blueprint §5).
//!
//! Phase 1 delivers the state PRIMITIVES, which are pure. The modules that
//! reconstruct pool state -- `live_state`, `ingestion`, `reconcile`,
//! `validation_select` -- import `quote_univ2`, so they move here in Phase 2
//! alongside `apex-math`. §6.1's dependency graph puts math before state; this
//! respects that rather than stubbing a premature pricing crate.

pub mod branch;
pub mod feed;
pub mod ordinal;
mod versioned;

pub use ordinal::Ordinal;
pub use versioned::{Snapshot, VerifiedState, Versioned};
