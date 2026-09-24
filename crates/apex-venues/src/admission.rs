//! Pool admission (PLAN.md §10.2, blueprint §6.3; conflicts C-11 and B-7).
//!
//! A pool that reaches the scanner has been *admitted*: somebody established
//! that the address holds code, that the venue filed against it is the venue
//! that deployed it, that its tokens and decimals were read from the chain
//! rather than from a file, and that its depth clears a threshold whose value
//! was measured.
//!
//! # Why this exists
//!
//! `base_venues_complete.yaml` is a machine-generated venue list containing
//! **fabricated router and quoter addresses** — `generate_base_venues.py`
//! invented them (B-7). Separately, 33 pools were filed under Uniswap V3 while
//! `factory()` said PancakeSwap owned them, including two of the deepest cheap
//! pools on Base. Quoting a PancakeSwap pool through the Uniswap quoter does
//! not error: the quoter resolves *its own* pool for that pair and fee tier and
//! prices a different pool entirely. The inventory looks fine and the number is
//! wrong.
//!
//! Both are the same failure — an address believed rather than checked — and
//! both are invisible downstream. Admission is where belief has to end.
//!
//! # The shape: two types, one direction
//!
//! [`PoolAdmissionRecord`] is what arrives: every field optional, because a
//! file can omit anything. [`PoolAdmission`] is what the engine consumes: every
//! field present, and constructible **only** by [`VenueRegistry::admit`]. The
//! same witness pattern as `DiscreteSize` and `VerifiedState` — you cannot hold
//! the verified type without having gone through the verification.

use apex_types::cost::GasLimit;
use apex_types::ids::{PoolId, TokenId, VenueId};
use apex_types::route::Exactness;
use alloy_primitives::{B256, U256};
use serde::{Deserialize, Serialize};

/// `keccak256("")` — what `extcodehash` returns for an address with no code.
///
/// An account that has never been touched returns zero instead, so BOTH are
/// "no contract here" and both must be refused.
pub const EMPTY_CODE_HASH: B256 = B256::new([
    0xc5, 0xd2, 0x46, 0x01, 0x86, 0xf7, 0x23, 0x3c, 0x92, 0x7e, 0x7d, 0xb2, 0xdc, 0xc7, 0x03, 0xc0,
    0xe5, 0x00, 0xb6, 0x53, 0xca, 0x82, 0x27, 0x3b, 0x7b, 0xfa, 0xd8, 0x04, 0x5d, 0x85, 0xa4, 0x70,
]);

/// Evidence that an address held code at a specific block.
///
/// Carried rather than fetched: admission is pure, and the read belongs to the
/// loader or to `scripts/data/verify_registry_bytecode.py`. A pure gate can be
/// tested exhaustively; one that reaches for a provider can only be tested
/// against whatever the node felt like returning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BytecodeEvidence {
    pub extcodehash: B256,
    pub observed_at_block: u64,
}

impl BytecodeEvidence {
    pub fn has_code(&self) -> bool {
        self.extcodehash != EMPTY_CODE_HASH && self.extcodehash != B256::ZERO
    }
}

/// How a pool's swap fee is determined.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeeBehavior {
    /// Fixed, in hundredths of a bip.
    Static { ppm: u32 },
    /// The venue changes it; the pool must be re-read. Slipstream pools at the
    /// same tick spacing were measured charging 212 and 2500 ppm.
    Dynamic,
    /// A Uniswap V4 hook decides. Phase 11.
    Hook { address: alloy_primitives::Address },
}

/// Which log stream rebuilds this pool's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconstructionMethod {
    /// `Swap`, `Mint`, `Burn` on a concentrated-liquidity pool.
    ConcentratedLiquidityLogs,
    /// `Sync` on a constant-product pair.
    ReserveSync,
    /// No log stream reconstructs it; state must be polled.
    PollOnly,
}

/// How deep the pool is, and whether that number may size capital.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DepthEstimate {
    /// Read from the chain: balances, or reserves, at a block.
    MeasuredOnChain { usd_micros: u128, at_block: u64 },
    /// From a third party (GeckoTerminal and friends). §18.5 forbids external
    /// feeds as truth, so this ranks and filters and never sizes.
    ExternalNonAuthoritative { usd_micros: u128 },
}

impl DepthEstimate {
    pub const fn usd_micros(&self) -> u128 {
        match self {
            Self::MeasuredOnChain { usd_micros, .. }
            | Self::ExternalNonAuthoritative { usd_micros } => *usd_micros,
        }
    }

    /// §18.5. An external number may decide what to *look at*; it may never
    /// decide how much to put at risk.
    pub const fn may_size_capital(&self) -> bool {
        matches!(self, Self::MeasuredOnChain { .. })
    }
}

/// Gas for one hop through this pool, and where the figure came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GasProfile {
    pub per_hop: GasLimit,
    /// False for every venue today: the six constants in the legacy
    /// `venues.rs` are round numbers with no recorded measurement. Phase 3.
    pub measured: bool,
}

/// What this pool has actually been observed to revert with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevertProfile {
    pub observed_executions: u32,
    pub observed_reverts: u32,
}

/// ERC-20 behaviour that breaks the "amount sent equals amount received"
/// assumption every AMM quote rests on (§7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferSemantics {
    Standard,
    FeeOnTransfer { bps: u32 },
    Rebasing,
    /// Nobody has classified this token yet.
    ///
    /// A legitimate answer, not a missing field — and one that keeps the pool
    /// out of live dispatch. Task 1.5's classifier is what replaces it.
    Unknown,
}

impl TransferSemantics {
    /// Whether a quote over this token is exact **as the pricing layer is
    /// written today**.
    ///
    /// Only `Standard`. The previous rule was "not `Unknown`", which let a
    /// *classified* fee-on-transfer token price as exact on the stated
    /// reasoning that "the engine can price around it". No engine does:
    /// nothing outside this module has ever read `FeeOnTransfer`'s `bps`, and
    /// no quote path applies a transfer fee. A token whose fee somebody had
    /// measured was therefore treated as more trustworthy than one nobody had
    /// measured, with the measurement unused.
    ///
    /// The match is exhaustive on purpose. A sixth variant cannot be added
    /// without deciding this question, and `FeeOnTransfer` may only move back
    /// to `true` in the same change that makes a quote apply its `bps` --
    /// `a_known_fee_on_transfer_token_is_still_shadow_only` is what fails
    /// otherwise.
    const fn prices_exactly(self) -> bool {
        match self {
            Self::Standard => true,
            // Eats part of every hop, and nothing subtracts it.
            Self::FeeOnTransfer { .. } => false,
            // Balances move without a transfer, so a quote's input can be stale
            // by the time it executes.
            Self::Rebasing => false,
            // Nobody has looked.
            Self::Unknown => false,
        }
    }
}

/// The unverified record, as a file or a builder produces it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PoolAdmissionRecord {
    pub pool: Option<PoolId>,
    pub venue: Option<VenueId>,
    pub bytecode: Option<BytecodeEvidence>,
    /// The factory that `factory()` reported, read from the pool itself.
    pub deployed_by: Option<alloy_primitives::Address>,
    pub tokens: Option<(TokenId, TokenId)>,
    pub decimals: Option<(u8, u8)>,
    pub fee_behavior: Option<FeeBehavior>,
    pub reconstruction: Option<ReconstructionMethod>,
    pub depth: Option<DepthEstimate>,
    pub gas_profile: Option<GasProfile>,
    pub revert_profile: Option<RevertProfile>,
    pub transfer_semantics: Option<(TransferSemantics, TransferSemantics)>,
    /// `topic0`s that mutate this pool. Empty is not the same as absent: a
    /// `PollOnly` pool legitimately has none, and `None` means nobody said.
    pub update_mapping: Option<Vec<B256>>,
}

/// Field names, in declaration order.
///
/// Used by the rejection test to null one field at a time. A field added to the
/// struct and not to this list would silently go untested, so
/// `the_required_field_list_covers_every_field` compares this against what
/// serde actually emits.
pub const REQUIRED_FIELDS: &[&str] = &[
    "pool",
    "venue",
    "bytecode",
    "deployed_by",
    "tokens",
    "decimals",
    "fee_behavior",
    "reconstruction",
    "depth",
    "gas_profile",
    "revert_profile",
    "transfer_semantics",
    "update_mapping",
];

/// A pool that passed admission. Every field present, construction private.
//
// clippy::manual_non_exhaustive suggests `#[non_exhaustive]` for the private
// `_sealed` field. That attribute says something different and weaker: it stops
// DOWNSTREAM crates using struct-literal syntax, as an API-evolution promise,
// while leaving construction open to every module in this crate. The private
// field says "you may not build one of these" to everyone except `admit`, which
// is the whole point -- holding a `PoolAdmission` is supposed to be proof that
// the verification ran. Same pattern as `DiscreteSize` and `VerifiedState`.
#[allow(clippy::manual_non_exhaustive)]
#[derive(Clone, Debug, PartialEq)]
pub struct PoolAdmission {
    pub pool: PoolId,
    pub venue: VenueId,
    pub bytecode: BytecodeEvidence,
    pub deployed_by: alloy_primitives::Address,
    pub tokens: (TokenId, TokenId),
    pub decimals: (u8, u8),
    pub fee_behavior: FeeBehavior,
    pub reconstruction: ReconstructionMethod,
    pub depth: DepthEstimate,
    pub gas_profile: GasProfile,
    pub revert_profile: RevertProfile,
    pub transfer_semantics: (TransferSemantics, TransferSemantics),
    pub update_mapping: Vec<B256>,
    /// Private: the only way to hold one of these is to have been admitted.
    _sealed: (),
}

impl PoolAdmission {
    /// INV-17 composed with §7.2: an admitted pool may still be shadow-only.
    ///
    /// A token that eats part of a hop -- whether because nobody classified it
    /// or because it is a known fee-on-transfer token nothing prices -- makes
    /// every downstream quote overstate its output. Ranking such a pool is
    /// fine; dispatching against it is not.
    pub const fn exactness(&self) -> Exactness {
        if self.transfer_semantics.0.prices_exactly() && self.transfer_semantics.1.prices_exactly() {
            Exactness::Proven
        } else {
            Exactness::Approximate
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    /// The record did not say. Never guessed at.
    MissingField(&'static str),
    /// `extcodehash` is the empty-code hash or zero.
    NoBytecode { address: alloy_primitives::Address },
    /// `factory()` disagrees with the venue the record claims.
    ///
    /// This is B-7's and the 33-pool misfiling's shared shape.
    WrongFactory {
        claimed: VenueId,
        deployed_by: alloy_primitives::Address,
    },
    /// The venue is not configured, so nothing can price this pool correctly.
    UnknownVenue(VenueId),
    /// Below the depth floor.
    TooShallow { usd_micros: u128, floor_micros: u128 },
}

/// The depth floor, with the measurement behind it.
///
/// §6.3: "Low-liquidity rejection is a configurable economic threshold, never a
/// magic number." The default is not a round number someone liked — it is the
/// output of `scripts/data/sweep_depth_floor.py`, run as a filter over one
/// event-census window so every floor faces identical conditions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepthPolicy {
    pub floor_usd_micros: u128,
    pub provenance: &'static str,
}

impl Default for DepthPolicy {
    fn default() -> Self {
        Self {
            floor_usd_micros: 100_000 * 1_000_000,
            provenance: "sweep_depth_floor.py over one event-census window: hit rate 17.4% at \
                         every floor from $0 to $50k and 20.0% at $100k, because a lower floor \
                         adds cycles but not instances -- the best cycle in an instance was \
                         already a deep one. $250k leaves 4 instances and finds nothing.",
        }
    }
}

/// The venues that may own a pool, and the factory each deploys from.
#[derive(Debug, Default)]
pub struct VenueRegistry {
    factories: Vec<(VenueId, alloy_primitives::Address)>,
    depth: DepthPolicy,
}

impl VenueRegistry {
    pub fn new(depth: DepthPolicy) -> Self {
        Self {
            factories: Vec::new(),
            depth,
        }
    }

    /// Register a venue's factory. A venue with no registered factory cannot
    /// admit anything, which is the fail-closed direction.
    pub fn with_factory(mut self, venue: VenueId, factory: alloy_primitives::Address) -> Self {
        self.factories.push((venue, factory));
        self
    }

    pub const fn depth_policy(&self) -> DepthPolicy {
        self.depth
    }

    /// Admit a pool, or say exactly why not.
    pub fn admit(&self, record: PoolAdmissionRecord) -> Result<PoolAdmission, AdmissionError> {
        macro_rules! require {
            ($field:ident) => {
                record
                    .$field
                    .ok_or(AdmissionError::MissingField(stringify!($field)))?
            };
        }

        let pool = require!(pool);
        let venue = require!(venue);
        let bytecode = require!(bytecode);
        let deployed_by = require!(deployed_by);
        let tokens = require!(tokens);
        let decimals = require!(decimals);
        let fee_behavior = require!(fee_behavior);
        let reconstruction = require!(reconstruction);
        let depth = require!(depth);
        let gas_profile = require!(gas_profile);
        let revert_profile = require!(revert_profile);
        let transfer_semantics = require!(transfer_semantics);
        let update_mapping = require!(update_mapping);

        if !bytecode.has_code() {
            return Err(AdmissionError::NoBytecode {
                address: pool.address,
            });
        }

        let Some((_, expected)) = self.factories.iter().find(|(v, _)| *v == venue) else {
            return Err(AdmissionError::UnknownVenue(venue));
        };
        if *expected != deployed_by {
            return Err(AdmissionError::WrongFactory {
                claimed: venue,
                deployed_by,
            });
        }

        if depth.usd_micros() < self.depth.floor_usd_micros {
            return Err(AdmissionError::TooShallow {
                usd_micros: depth.usd_micros(),
                floor_micros: self.depth.floor_usd_micros,
            });
        }

        Ok(PoolAdmission {
            pool,
            venue,
            bytecode,
            deployed_by,
            tokens,
            decimals,
            fee_behavior,
            reconstruction,
            depth,
            gas_profile,
            revert_profile,
            transfer_semantics,
            update_mapping,
            _sealed: (),
        })
    }
}

/// Convenience for callers that only have a raw `U256` extcodehash.
impl From<(U256, u64)> for BytecodeEvidence {
    fn from((hash, block): (U256, u64)) -> Self {
        Self {
            extcodehash: B256::from(hash.to_be_bytes::<32>()),
            observed_at_block: block,
        }
    }
}

/// INV-40. Four of the five mean the same thing economically: this pool cannot
/// be priced correctly, so no trade through it is admissible. `TooShallow` is
/// different -- the pool is fine and the opportunity is not.
impl apex_types::miss::ExplainsMiss for AdmissionError {
    fn miss_reason(&self) -> apex_types::miss::MissReason {
        use apex_types::miss::MissReason as R;
        match self {
            Self::MissingField(_)
            | Self::NoBytecode { .. }
            | Self::WrongFactory { .. }
            | Self::UnknownVenue(_) => R::VenueDisabled,
            Self::TooShallow { .. } => R::LowEv,
        }
    }
}
