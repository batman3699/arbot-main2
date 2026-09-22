//! Route identity and the complexity metric (Blueprint §13).

use crate::ids::{PoolId, TokenId, VenueId};
use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RouteHop {
    pub venue: VenueId,
    pub pool: PoolId,
    pub token_in: TokenId,
    pub token_out: TokenId,
    pub fee_ppm: u32,
}

/// §13: "hop count is NOT the true complexity metric."
///
/// This repo has measured that directly -- a ~1,200-sample census across 2/3/4
/// hop routes found deeper routes monotonically worse, which a hop counter
/// describes but does not explain. These are the terms that do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComplexityCost {
    pub hops: u8,
    pub external_calls: u16,
    pub calldata_bytes: u32,
    pub state_deps: u16,
    pub tick_crossings: u32,
    pub hooks: u8,
    pub gas_estimate: u64,
    pub failure_surface: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteCommitment {
    pub hops: Vec<RouteHop>,
    pub complexity_cost: ComplexityCost,
    /// keccak over the normalized hop list; the dedup key downstream (§25).
    pub route_hash: B256,
}

/// §16.2: never silently promote a heuristic allocation to "optimal".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CertificateStatus {
    Proven,
    Heuristic,
    InvalidForCertification,
}

/// §11 / INV-17: an engine not proven exact produces candidate-only output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exactness {
    Proven,
    Approximate,
}

impl Exactness {
    /// The risk gate's predicate. An `Approximate` quote may rank and propose;
    /// it may not authorize a live dispatch.
    pub const fn may_authorize_live_dispatch(self) -> bool {
        matches!(self, Self::Proven)
    }
}
