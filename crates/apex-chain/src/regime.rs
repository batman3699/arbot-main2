//! Runtime regime discovery (§20.1, Blueprint §4.3, §4.5).
//!
//! > Adapters **discover** their active execution regime at startup and
//! > periodically thereafter rather than hard-coding a historical assumption.
//! > A chain whose regime cannot be discovered is **not admitted to live
//! > trading.**
//!
//! The second sentence is the whole design. [`RegimeDiscovery`] has no variant
//! that means "assume the usual", and [`ChainRegime`] cannot be constructed
//! from a literal outside this module — so a chain that did not answer cannot
//! be traded by a caller who forgot to check.

use apex_types::ids::ChainId;
use apex_types::time::{DurationNanos, UnixNanos};
use serde::{Deserialize, Serialize};

/// §4.3: how the chain orders what it accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderingMode {
    /// A single sequencer decides, first-come within its own rules. Base.
    Sequencer,
    /// A builder market auctions ordering. Ethereum L1.
    BuilderAuction,
    /// Ordering is fixed by protocol, not bought.
    ProtocolFixed,
}

/// §4.5: what a priority fee actually buys here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PriorityFeeSemantics {
    /// It ranks you against other transactions in the same window.
    RanksWithinWindow,
    /// It is paid but does not affect ordering.
    PaidButNotOrdering,
    /// There is no priority fee.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplacementRules {
    /// A replacement must raise the fee by at least this many basis points.
    BumpRequired { min_bump_bps: u32 },
    /// Replacement is not supported.
    NotSupported,
}

/// §23: which fee components exist here at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeModel {
    /// An L2 that posts data to an L1 pays a data fee the L1 sets.
    pub has_l1_data_fee: bool,
    /// Blob-denominated L1 data pricing (Fjord and later on OP Stack).
    pub l1_data_fee_uses_blobs: bool,
    pub has_priority_fee: bool,
    /// A payment to whoever builds the block, separate from the priority fee.
    pub has_builder_payment: bool,
}

/// §20.1's seven fields. **Constructible only through [`RegimeDiscovery`]**:
/// the private marker means a caller cannot write one down and call it
/// discovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainRegime {
    pub chain: ChainId,
    pub ordering_mode: OrderingMode,
    pub priority_fee_semantics: PriorityFeeSemantics,
    /// One block, or one preconfirmation round where those are shorter.
    pub round_length: DurationNanos,
    pub fast_feed_available: bool,
    pub private_feed_available: bool,
    pub replacement_rules: ReplacementRules,
    pub gas_and_data_fee_model: FeeModel,
    /// When this was observed. A regime is a measurement, and a measurement has
    /// an age.
    pub discovered_at: UnixNanos,
    #[serde(skip)]
    _discovered: Discovered,
}

/// Zero-sized, private field, no public constructor. See [`ChainRegime`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovered(());

/// Why a chain could not be admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotDiscovered {
    /// The probe did not answer.
    Unreachable { detail: String },
    /// It answered, but with something that does not describe a regime this
    /// system can trade against.
    Unrecognized { detail: String },
    /// It answered once and the answer has since aged past its TTL. A stale
    /// regime is not a regime: chain parameters change, which is the entire
    /// reason this is discovered rather than hard-coded.
    Stale { age: DurationNanos, ttl: DurationNanos },
}

impl std::fmt::Display for NotDiscovered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable { detail } => write!(f, "regime probe failed: {detail}"),
            Self::Unrecognized { detail } => write!(f, "unrecognized regime: {detail}"),
            Self::Stale { age, ttl } => {
                write!(f, "regime is {} ns old; the TTL is {} ns", age.0, ttl.0)
            }
        }
    }
}

impl std::error::Error for NotDiscovered {}

/// What a probe returns. There is deliberately no `Assumed` variant.
#[derive(Clone, Debug, PartialEq)]
pub enum RegimeDiscovery {
    Discovered(Box<ChainRegime>),
    Failed(NotDiscovered),
}

impl RegimeDiscovery {
    /// The only constructor of a [`ChainRegime`].
    #[allow(clippy::too_many_arguments)]
    pub fn discovered(
        chain: ChainId,
        ordering_mode: OrderingMode,
        priority_fee_semantics: PriorityFeeSemantics,
        round_length: DurationNanos,
        fast_feed_available: bool,
        private_feed_available: bool,
        replacement_rules: ReplacementRules,
        gas_and_data_fee_model: FeeModel,
        discovered_at: UnixNanos,
    ) -> Self {
        Self::Discovered(Box::new(ChainRegime {
            chain,
            ordering_mode,
            priority_fee_semantics,
            round_length,
            fast_feed_available,
            private_feed_available,
            replacement_rules,
            gas_and_data_fee_model,
            discovered_at,
            _discovered: Discovered(()),
        }))
    }

    /// §20.1's admission rule, and the only way to get a regime out.
    ///
    /// Takes `now` and a TTL because the rule is "discovered at startup **and
    /// periodically thereafter**" — a regime discovered once and never
    /// re-checked is a hard-coded assumption with extra steps.
    pub fn admit_to_live_trading(
        &self,
        now: UnixNanos,
        ttl: DurationNanos,
    ) -> Result<&ChainRegime, NotDiscovered> {
        match self {
            Self::Failed(e) => Err(e.clone()),
            Self::Discovered(r) => {
                let age = DurationNanos(now.0.saturating_sub(r.discovered_at.0));
                if age.0 > ttl.0 {
                    return Err(NotDiscovered::Stale { age, ttl });
                }
                Ok(r)
            }
        }
    }
}
