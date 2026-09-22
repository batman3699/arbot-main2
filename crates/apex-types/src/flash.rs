//! Flash liquidity as a routed resource (Blueprint §19).

use crate::ids::{FlashProviderId, PoolId, TokenId};
use alloy_primitives::U256;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackConstraints {
    /// Some lenders require repayment by approval, others by transfer.
    pub repay_by_transfer: bool,
    pub reentrancy_permitted: bool,
    pub max_callback_gas: u64,
}

/// Blueprint §19.1, all nine fields. §1.2: a flash loan is financing, not edge --
/// these fields exist so the router can price that financing, not to make a
/// losing route look profitable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlashSourceQuote {
    pub provider: FlashProviderId,
    pub asset: TokenId,
    pub amount: U256,
    pub premium: U256,
    pub gas_overhead: u64,
    pub callback_constraints: CallbackConstraints,
    pub availability_probability: f64,
    pub state_dependencies: Vec<PoolId>,
    pub reliability_score: f64,
}
