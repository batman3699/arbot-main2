//! Live pool state, maintained from decoded logs.
//!
//! In Phase 1 this store is WRITTEN and MEASURED but never read for pricing.
//! `ScanSnapshot` (spec §4.2) and the candidate staleness guards (§6) arrive
//! with Phase 2, when something finally reads it.

use crate::continuity::Ordinal;
use crate::quote_univ2::UniV2PairState;
use ethers::types::U256;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnknownReason {
    NeverAnchored,
    ContinuityBreak,
    Reorg,
    WsUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleReason {
    AnchorTtlExpired,
    DriftBudgetExhausted,
}

/// Why a pool may or may not be priced from local state.
///
/// `Diverged` carries its magnitude: a pool measured wrong is a different
/// condition from one that merely aged out, and the size of the disagreement is
/// what tells a decoder bug from an RPC timing artefact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustState {
    Anchored,
    Derived,
    Stale(StaleReason),
    Diverged { err_bps: i64 },
    Unknown(UnknownReason),
}

/// May this state be used to price a route?
///
/// Exhaustive on purpose — NO wildcard arm. A new `TrustState` variant must
/// fail to compile here until someone decides its policy.
pub fn may_price_locally(trust: &TrustState) -> bool {
    match trust {
        TrustState::Anchored | TrustState::Derived => true,
        TrustState::Stale(_) => false,
        TrustState::Diverged { .. } => false,
        TrustState::Unknown(_) => false,
    }
}

/// Where a snapshot came from and what lineage it belongs to.
#[derive(Clone, Copy, Debug)]
pub struct Provenance {
    /// Monotonic per pool, bumped on every accepted update.
    pub state_version: u64,
    /// Identity of the RPC anchor this lineage descends from.
    pub anchor_id: u64,
    /// Global epoch at application time; a break invalidates every snapshot
    /// carrying an older value, in O(1).
    pub continuity_epoch: u64,
    /// Cursor position of the log that produced this, `None` for an anchor.
    pub ordinal: Option<Ordinal>,
    pub anchored_at: Instant,
    pub trust: TrustState,
}

#[derive(Clone, Debug)]
pub struct V2Snapshot {
    pub state: UniV2PairState,
    pub prov: Provenance,
}

/// CL state as carried by a `Swap` log — exactly what `slot0()` plus
/// `liquidity()` return, which is what makes it checkable against RPC.
/// Tick ladders and balances arrive in Phase 2.
#[derive(Clone, Debug)]
pub struct ClSnapshot {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub prov: Provenance,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy must be an exhaustive match with no wildcard, so adding a
    /// trust state without deciding its policy fails to COMPILE rather than
    /// silently defaulting to tradable.
    #[test]
    fn only_anchored_and_derived_may_price_locally() {
        assert!(may_price_locally(&TrustState::Anchored));
        assert!(may_price_locally(&TrustState::Derived));
        assert!(!may_price_locally(&TrustState::Stale(
            StaleReason::AnchorTtlExpired
        )));
        assert!(!may_price_locally(&TrustState::Stale(
            StaleReason::DriftBudgetExhausted
        )));
        assert!(!may_price_locally(&TrustState::Diverged { err_bps: 1 }));
        assert!(!may_price_locally(&TrustState::Unknown(
            UnknownReason::NeverAnchored
        )));
        assert!(!may_price_locally(&TrustState::Unknown(
            UnknownReason::ContinuityBreak
        )));
        assert!(!may_price_locally(&TrustState::Unknown(UnknownReason::Reorg)));
        assert!(!may_price_locally(&TrustState::Unknown(
            UnknownReason::WsUnavailable
        )));
    }

    /// Stale is NOT a flavour of Derived. Conflating them is how a pool that
    /// aged out keeps getting priced locally.
    #[test]
    fn stale_is_not_derived() {
        assert_ne!(
            may_price_locally(&TrustState::Derived),
            may_price_locally(&TrustState::Stale(StaleReason::AnchorTtlExpired))
        );
    }
}
