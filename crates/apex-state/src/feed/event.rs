//! What the state feed emits, and the plane reacts to (§5.2, §12.4, §46.3).
//!
//! # Why this is here and not in `apex-runtime`
//!
//! Task 8.4 defined `StateEvent` in `apex-runtime::bus`, which was wrong in a way
//! that only showed up when `apex-search` came to exist. §6.1's dependency graph
//! runs `apex-state → apex-venues/apex-chain → apex-search → … → apex-runtime`:
//! the runtime is at the bottom and depends on the search, so a `CandidateSource`
//! implemented in `apex-search` cannot take a type defined in `apex-runtime`.
//!
//! `apex_types::ack` already made this argument for `LifecycleStage` — *"the
//! controller calls adapters, so the adapter crate cannot depend on the
//! controller, which leaves exactly one honest place for the shared words"* — and
//! the same reasoning points here rather than at `apex-types`: a state event
//! carries a [`StateFingerprint`] and an [`Ordinal`], and both are this crate's
//! vocabulary. Putting it in `apex-types` would move `Ordinal` too, and `Ordinal`
//! is where it is for reasons its own module header explains.
//!
//! The bus, the two lanes and the loss counters stay in `apex-runtime`. They are
//! plumbing, not vocabulary — the same split `AckLadder` and `LifecycleStage`
//! already have.
//!
//! `scripts/ci/crate_dependency_direction.sh` now holds the line, because cargo
//! does not: cargo refuses a dependency *cycle*, and a single wrong-direction
//! edge is not a cycle. `apex-search → apex-runtime` would have compiled.

use crate::ordinal::Ordinal;
use apex_types::ids::ChainId;
use apex_types::state::StateFingerprint;
use apex_types::time::UnixNanos;
use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

/// A market state observation worth reacting to.
///
/// Serialisable because the stream is **data**. `apex-runtime`'s
/// `tests/fixtures/recorded_stream.json` holds one and replays it; Task 8.5's
/// 14-day shadow run replaces the file rather than the test. A stream expressed
/// as a Rust literal could not be swapped for a real capture without rewriting
/// whatever asserts against it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateEvent {
    pub chain: ChainId,
    /// Position in the chain's total order (§5.2). Two feeds delivering the same
    /// observation agree here, which is what makes redelivery detectable.
    pub at: Ordinal,
    pub observed_at: UnixNanos,
    pub fingerprint: StateFingerprint,
    pub kind: EventKind,
}

impl StateEvent {
    /// The pools whose state this event moved, when it says.
    ///
    /// The frontier's revaluation key (§12.1): an event that names no pool can
    /// only trigger broad discovery, which is the slow path. Returning an empty
    /// slice rather than "all pools" is deliberate — a `Block` event does move
    /// pools, but it does not say *which*, and answering "all of them" would make
    /// every sealed block a full rebuild.
    pub fn touched_pools(&self) -> &[apex_types::ids::PoolId] {
        match &self.kind {
            EventKind::Block | EventKind::Preconfirmation => &[],
            EventKind::PendingSwap { pools, .. }
            | EventKind::LiquidityChange { pools, .. }
            | EventKind::TickTransition { pools }
            | EventKind::FeeChange { pools }
            | EventKind::HookMutation { pools }
            | EventKind::OracleMutation { pools }
            | EventKind::StableDislocation { pools }
            | EventKind::Liquidation { pools } => pools,
        }
    }
}

/// §12.4's event classes.
///
/// **Eight template classes plus the two block-level kinds**, and the eight are
/// §12.4's own list: large swap, liquidity removal/addition, liquidation,
/// oracle-sensitive mutation, stablecoin dislocation, tick transition, hook state
/// mutation, fee-tier/dynamic-fee change.
///
/// The measured reason this enum matters: event-triggered sampling produced this
/// repository's first net-positive arbitrage samples, and **88% of them followed
/// a swap**. Continuous sampling of quiet blocks measures the wrong moment. So
/// `PendingSwap` is not one variant among ten — it is the one the evidence is
/// about, and it carries the notional that decides whether it is worth reacting
/// to at all.
///
/// No `Eq`: `notional_usd` is an `f64`, and a float has no total equality. That
/// is a fact about the measurement rather than a limitation — two swaps whose
/// notionals differ by a rounding step are not "the same event", and an `Eq` that
/// said otherwise would be the wrong answer quietly. Event identity for
/// deduplication is `(chain, at)`, which is exactly what `Ordinal` is for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    /// A sealed block. Moves state without saying which pools.
    Block,
    /// A preconfirmation payload — a Base flashblock (§22).
    Preconfirmation,
    /// §12.4's "large swap". The census that found the edge triggered on
    /// swaps ≥ $5,000, and `notional_usd` is carried so the threshold is a
    /// decision the caller makes rather than one baked in here.
    PendingSwap {
        target: B256,
        pools: Vec<apex_types::ids::PoolId>,
        notional_usd: Option<f64>,
    },
    /// §12.4's "liquidity removal/addition". `added` distinguishes them, because
    /// removal tightens depth and addition loosens it and the two move the
    /// opportunity in opposite directions.
    LiquidityChange { pools: Vec<apex_types::ids::PoolId>, added: bool },
    Liquidation { pools: Vec<apex_types::ids::PoolId> },
    OracleMutation { pools: Vec<apex_types::ids::PoolId> },
    StableDislocation { pools: Vec<apex_types::ids::PoolId> },
    TickTransition { pools: Vec<apex_types::ids::PoolId> },
    HookMutation { pools: Vec<apex_types::ids::PoolId> },
    FeeChange { pools: Vec<apex_types::ids::PoolId> },
}

impl EventKind {
    /// §12.4's eight, in the order the plan lists them. Block and
    /// preconfirmation are not template classes — they carry no pool set, so
    /// there is nothing for a template to be keyed on.
    pub const TEMPLATE_CLASSES: [EventClass; 8] = [
        EventClass::LargeSwap,
        EventClass::LiquidityChange,
        EventClass::Liquidation,
        EventClass::OracleMutation,
        EventClass::StableDislocation,
        EventClass::TickTransition,
        EventClass::HookMutation,
        EventClass::FeeChange,
    ];

    /// Which §12.4 class this is, or `None` for the two block-level kinds.
    ///
    /// Matched exhaustively: a tenth variant cannot be added without deciding
    /// whether it is a template class, which is the question Engine D is built
    /// around.
    pub const fn class(&self) -> Option<EventClass> {
        match self {
            Self::Block | Self::Preconfirmation => None,
            Self::PendingSwap { .. } => Some(EventClass::LargeSwap),
            Self::LiquidityChange { .. } => Some(EventClass::LiquidityChange),
            Self::Liquidation { .. } => Some(EventClass::Liquidation),
            Self::OracleMutation { .. } => Some(EventClass::OracleMutation),
            Self::StableDislocation { .. } => Some(EventClass::StableDislocation),
            Self::TickTransition { .. } => Some(EventClass::TickTransition),
            Self::HookMutation { .. } => Some(EventClass::HookMutation),
            Self::FeeChange { .. } => Some(EventClass::FeeChange),
        }
    }
}

/// §12.4's eight template classes, as a key.
///
/// `Ord` is derived so this can key a `BTreeMap` and iterate deterministically.
/// As with `MissReason` and `LossClass`, **the order is a map key and not a
/// ranking** — nothing may take a max over it, and no class is "bigger" than
/// another. `LargeSwap` being first is §12.4's listing order, not a priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EventClass {
    LargeSwap,
    LiquidityChange,
    Liquidation,
    OracleMutation,
    StableDislocation,
    TickTransition,
    HookMutation,
    FeeChange,
}

impl EventClass {
    pub const fn label(self) -> &'static str {
        match self {
            Self::LargeSwap => "large_swap",
            Self::LiquidityChange => "liquidity_change",
            Self::Liquidation => "liquidation",
            Self::OracleMutation => "oracle_mutation",
            Self::StableDislocation => "stable_dislocation",
            Self::TickTransition => "tick_transition",
            Self::HookMutation => "hook_mutation",
            Self::FeeChange => "fee_change",
        }
    }
}
