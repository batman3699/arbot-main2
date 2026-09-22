//! The `VenueAdapter` contract (PLAN.md §10.4, blueprint §8.3).
//!
//! "**No adapter may hide a meaningful economic assumption from the core
//! engine**" — enforced by `gas_model` and `classify_revert` being required
//! methods with no default implementation.
//!
//! # Method set: five now, two later, none by default
//!
//! §10.4 sketches seven methods. Two of them, `simulate_call_graph` and
//! `encode_exact`, are typed over `CallGraph` and `EncodedAction`, which belong
//! to Phase 4 (simulation) and Phase 5 (settlement). Inventing those shapes now
//! would mean guessing at two phases' worth of design and then contradicting
//! the guess. They are not declared here.
//!
//! The risk in leaving them out is obvious and worth naming: when Phase 4 adds
//! `simulate_call_graph`, the path of least resistance is to give it a default
//! body so the existing adapters keep compiling — which is exactly the
//! "inherits an answer nobody checked" failure the no-defaults rule guards
//! against. `scripts/ci/no_adapter_defaults.sh` fails the build if any method
//! in this trait ever grows a body, so the rule holds for methods that do not
//! exist yet.
//!
//! What IS here is the whole *economic* surface: what it costs, what it
//! quotes, how exact that quote is, what it read, and what its failures mean.

use crate::revert::classify_revert;
use apex_math::engine::{
    ClEdgeState, ClEngine, CpmmEngine, CpmmState, ExactPricingEngine, ExactQuote, Order,
    PricingError, PricingResult, SolidlyEngine, SolidlyState,
};
use apex_types::cost::GasLimit;
use apex_types::ids::{PoolId, VenueId};
use apex_types::route::Exactness;
use apex_types::sim::RevertClass;

/// Venue identifiers. Stable: they appear in metrics, breakers and tickets.
pub mod venue_ids {
    use apex_types::ids::VenueId;
    pub const UNISWAP_V3: VenueId = VenueId(1);
    pub const AERODROME_SLIPSTREAM: VenueId = VenueId(2);
    pub const PANCAKESWAP_V3: VenueId = VenueId(3);
    pub const UNISWAP_V2: VenueId = VenueId(4);
    pub const AERODROME_VOLATILE: VenueId = VenueId(5);
    pub const CURVE: VenueId = VenueId(6);
    pub const BALANCER: VenueId = VenueId(7);
    pub const UNISWAP_V4: VenueId = VenueId(8);
}

/// Pool state, in whichever shape its venue family needs.
///
/// A concrete enum rather than an associated type, because the trait is used
/// as `dyn VenueAdapter` by the router and an associated type in argument
/// position is not object-safe.
#[derive(Clone, Debug)]
pub enum VenueState {
    ConcentratedLiquidity(Box<ClEdgeState>),
    ConstantProduct(CpmmState),
    Solidly(Box<SolidlyState>),
}

/// What a venue reads, structurally, in order to be priced.
///
/// Distinct from `apex_math::engine::StateDeps`, which answers a different
/// question: that one names the ticks a PARTICULAR quote rested on, and needs
/// the state to answer. This one is a property of the venue and is answerable
/// from the pool id alone, which is what a state-reconstruction planner needs
/// before it has fetched anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateNeeds {
    pub pool: PoolId,
    /// Current price or reserves.
    pub reads_spot: bool,
    /// Active liquidity, separate from price.
    pub reads_liquidity: bool,
    /// The tick bitmap and the `liquidity_net` of initialized ticks.
    pub reads_tick_bitmap: bool,
    /// The pool's real ERC-20 balances, which bound what it can pay out.
    pub reads_token_balances: bool,
}

/// Where a gas figure came from.
///
/// Gas is a first-order term in the profit decision, so an unmeasured gas
/// number is a hidden economic assumption — precisely what §8.3 forbids. The
/// six per-venue constants inherited from `venues.rs` are round numbers with no
/// recorded measurement, and this type makes that visible to the caller instead
/// of leaving it in a comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GasProvenance {
    /// Measured from real executions. `source` names where the measurement
    /// lives so it can be re-run.
    Measured { samples: u32, source: &'static str },
    /// Inherited from the legacy constant with no recorded measurement.
    ///
    /// A venue on this provenance may rank and propose. Phase 3's cost model
    /// is where these get measured; until then, treating them as known is the
    /// assumption the caller is entitled to see.
    UnmeasuredLegacyConstant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GasModel {
    /// Gas for one hop through this venue, excluding the transaction's own
    /// intrinsic cost and any flash-loan wrapper.
    pub per_hop: GasLimit,
    pub provenance: GasProvenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdapterQuote {
    pub quote: ExactQuote,
    /// INV-17: an engine not proven exact produces candidate-only output.
    pub exactness: Exactness,
}

/// The venue contract. **No method has a default implementation.**
pub trait VenueAdapter: Send + Sync {
    fn venue_id(&self) -> VenueId;

    fn identify_state_dependencies(&self, pool: PoolId) -> StateNeeds;

    fn quote_exact(&self, state: &VenueState, order: &Order) -> PricingResult<AdapterQuote>;

    fn gas_model(&self) -> GasModel;

    fn classify_revert(&self, data: &[u8]) -> RevertClass;
}

/// The state kind an adapter was handed did not match the venue.
const WRONG_STATE: &str = "venue state kind does not match this adapter";

/// Venues this repository can talk to but cannot price locally.
///
/// `quote_curve` and `quote_balancer` are `abigen!` RPC clients: they ask the
/// chain and report the answer. There is no local implementation of either
/// curve here, so there is nothing to make exact and nothing to differential
/// against. Returning `NotRepresentable` is the honest answer; a quote that
/// forwards the on-chain quoter's number and calls itself exact would make
/// INV-16 ("no router quote is authoritative") a dead letter.
const NO_LOCAL_MATHS: &str =
    "no local implementation of this curve exists; the on-chain quoter is not authoritative";

macro_rules! cl_adapter {
    ($name:ident, $venue:path, $gas:expr, $doc:expr) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Default)]
        pub struct $name;

        impl VenueAdapter for $name {
            fn venue_id(&self) -> VenueId {
                $venue
            }

            fn identify_state_dependencies(&self, pool: PoolId) -> StateNeeds {
                StateNeeds {
                    pool,
                    reads_spot: true,
                    reads_liquidity: true,
                    reads_tick_bitmap: true,
                    // `ClPoolState::balance0/1`: the pool's `liquidity()` is a
                    // VIRTUAL reserve of a curve running 0..infinity, measured
                    // on Base to overstate real holdings by 16-56x on deep
                    // pools. The balances are the only sound capacity bound.
                    reads_token_balances: true,
                }
            }

            fn quote_exact(
                &self,
                state: &VenueState,
                order: &Order,
            ) -> PricingResult<AdapterQuote> {
                let VenueState::ConcentratedLiquidity(cl) = state else {
                    return Err(PricingError::NotRepresentable(WRONG_STATE));
                };
                Ok(AdapterQuote {
                    quote: ClEngine.quote_exact(cl, order)?,
                    // Proven: `cl_swap` crosses ticks against a ladder that
                    // states the range it covers, and refuses outside it.
                    exactness: Exactness::Proven,
                })
            }

            fn gas_model(&self) -> GasModel {
                GasModel {
                    per_hop: GasLimit($gas),
                    provenance: GasProvenance::UnmeasuredLegacyConstant,
                }
            }

            fn classify_revert(&self, data: &[u8]) -> RevertClass {
                classify_revert(data)
            }
        }
    };
}

cl_adapter!(
    UniV3Adapter,
    venue_ids::UNISWAP_V3,
    140_000,
    "Uniswap V3. `ESTIMATED_GAS_UNIV3` in the legacy `venues.rs`."
);
cl_adapter!(
    SlipstreamAdapter,
    venue_ids::AERODROME_SLIPSTREAM,
    140_000,
    "Aerodrome Slipstream. Same swap loop and same gas as Uniswap V3; its pool key is a tick spacing rather than a fee tier, which the loader resolves before an adapter sees the state."
);
cl_adapter!(
    PancakeV3Adapter,
    venue_ids::PANCAKESWAP_V3,
    140_000,
    "PancakeSwap V3."
);

/// Uniswap V2 and its constant-product forks.
#[derive(Clone, Copy, Debug, Default)]
pub struct UniV2Adapter;

impl VenueAdapter for UniV2Adapter {
    fn venue_id(&self) -> VenueId {
        venue_ids::UNISWAP_V2
    }

    fn identify_state_dependencies(&self, pool: PoolId) -> StateNeeds {
        StateNeeds {
            pool,
            reads_spot: true,
            // A constant-product pool's reserves ARE its liquidity and ARE its
            // balances. One read answers all three, and saying so is how a
            // reconstruction planner avoids three round trips for one fact.
            reads_liquidity: false,
            reads_tick_bitmap: false,
            reads_token_balances: false,
        }
    }

    fn quote_exact(&self, state: &VenueState, order: &Order) -> PricingResult<AdapterQuote> {
        let VenueState::ConstantProduct(cp) = state else {
            return Err(PricingError::NotRepresentable(WRONG_STATE));
        };
        Ok(AdapterQuote {
            quote: CpmmEngine.quote_exact(cp, order)?,
            exactness: Exactness::Proven,
        })
    }

    fn gas_model(&self) -> GasModel {
        GasModel {
            per_hop: GasLimit(130_000),
            provenance: GasProvenance::UnmeasuredLegacyConstant,
        }
    }

    fn classify_revert(&self, data: &[u8]) -> RevertClass {
        classify_revert(data)
    }
}

/// Aerodrome's volatile and stable pools (the Solidly curves).
#[derive(Clone, Copy, Debug, Default)]
pub struct AerodromeAdapter;

impl VenueAdapter for AerodromeAdapter {
    fn venue_id(&self) -> VenueId {
        venue_ids::AERODROME_VOLATILE
    }

    fn identify_state_dependencies(&self, pool: PoolId) -> StateNeeds {
        StateNeeds {
            pool,
            reads_spot: true,
            reads_liquidity: false,
            reads_tick_bitmap: false,
            reads_token_balances: false,
        }
    }

    fn quote_exact(&self, state: &VenueState, order: &Order) -> PricingResult<AdapterQuote> {
        let VenueState::Solidly(s) = state else {
            return Err(PricingError::NotRepresentable(WRONG_STATE));
        };
        Ok(AdapterQuote {
            quote: SolidlyEngine.quote_exact(s, order)?,
            // The stable curve solves x^3y + xy^3 = k by Newton iteration with
            // a bounded step count. It converges or it refuses; it does not
            // approximate. The volatile curve is plain constant product.
            exactness: Exactness::Proven,
        })
    }

    fn gas_model(&self) -> GasModel {
        GasModel {
            per_hop: GasLimit(135_000),
            provenance: GasProvenance::UnmeasuredLegacyConstant,
        }
    }

    fn classify_revert(&self, data: &[u8]) -> RevertClass {
        classify_revert(data)
    }
}

macro_rules! unpriceable_adapter {
    ($name:ident, $venue:path, $gas:expr, $doc:expr) => {
        #[doc = $doc]
        ///
        /// Has an adapter so the venue is visible to the registry, the breaker
        /// and the metrics — and so the absence of local maths is a value the
        /// caller receives rather than a venue that silently is not there.
        #[derive(Clone, Copy, Debug, Default)]
        pub struct $name;

        impl VenueAdapter for $name {
            fn venue_id(&self) -> VenueId {
                $venue
            }

            fn identify_state_dependencies(&self, pool: PoolId) -> StateNeeds {
                StateNeeds {
                    pool,
                    reads_spot: true,
                    reads_liquidity: true,
                    reads_tick_bitmap: false,
                    reads_token_balances: true,
                }
            }

            fn quote_exact(
                &self,
                _state: &VenueState,
                _order: &Order,
            ) -> PricingResult<AdapterQuote> {
                Err(PricingError::NotRepresentable(NO_LOCAL_MATHS))
            }

            fn gas_model(&self) -> GasModel {
                GasModel {
                    per_hop: GasLimit($gas),
                    provenance: GasProvenance::UnmeasuredLegacyConstant,
                }
            }

            fn classify_revert(&self, data: &[u8]) -> RevertClass {
                classify_revert(data)
            }
        }
    };
}

unpriceable_adapter!(
    CurveAdapter,
    venue_ids::CURVE,
    180_000,
    "Curve. `quote_curve` is an `abigen!` client for `get_dy`; there is no local StableSwap implementation in this repository."
);
unpriceable_adapter!(
    BalancerAdapter,
    venue_ids::BALANCER,
    155_000,
    "Balancer. `quote_balancer` is an `abigen!` client for `queryBatchSwap`; there is no local weighted-pool implementation here."
);

/// Every adapter this crate ships, for the registry and for tests that must
/// enumerate them.
pub fn all_adapters() -> Vec<Box<dyn VenueAdapter>> {
    vec![
        Box::new(UniV3Adapter),
        Box::new(SlipstreamAdapter),
        Box::new(PancakeV3Adapter),
        Box::new(UniV2Adapter),
        Box::new(AerodromeAdapter),
        Box::new(CurveAdapter),
        Box::new(BalancerAdapter),
    ]
}
