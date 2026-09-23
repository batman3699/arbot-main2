//! Multi-lane signer pool (§18.2, §27.5, §27.6). INV-04.

pub mod nonce;
pub mod pool;

pub use nonce::{NonceError, NonceLane, ReservedNonce, STALE_RESERVATION};
pub use pool::{
    ExecutorAuth, LaneAssignment, LaneConfig, LaneHealth, LaneRequirements, NoLane, SignerPool,
};
