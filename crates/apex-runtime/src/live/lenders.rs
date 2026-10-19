//! The flash lenders a cycle borrows from (Task 8.5 R27, R29).
//!
//! The lean executor borrows WETH from either, with no fee:
//!
//! - Morpho Blue, called directly: ~82,679 WETH (2026-10-13);
//! - Balancer V2's vault: ~19 WETH.
//!
//! Measured on a fork of Base (R29, block 52,671,989, ticket 664's route,
//! dusted), Morpho's loan is the cheaper: 348,647 gas against Balancer's
//! 361,294. So a size borrows from the cheapest lender that holds enough of it:
//! Morpho at every size it covers, Balancer only if it does not.

use apex_exec::lean::LeanLender;
use apex_types::ids::FlashProviderId;
use alloy_primitives::{address, Address};
use ethers_core::types::U256;

use crate::live::frontier::BALANCER_FLASH;

/// Morpho Blue's loans: the next id after Balancer's.
pub const MORPHO_FLASH: FlashProviderId = FlashProviderId(2);

/// Morpho Blue on Base: the lender itself, since R29.
pub const MORPHO_BLUE: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");

/// What a Balancer loan costs over Morpho's: the fork's 12,647, rounded up.
/// The settlement gas model (`live::gas::LEAN`) is measured with Morpho's.
pub const BALANCER_EXTRA_GAS: u64 = 12_700;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LenderChoice {
    pub id: FlashProviderId,
    pub lender: LeanLender,
    /// The gas this lender adds over the model's (Morpho's).
    pub extra_gas: u64,
}

pub const MORPHO: LenderChoice = LenderChoice { id: MORPHO_FLASH, lender: LeanLender::Morpho, extra_gas: 0 };

pub const BALANCER: LenderChoice =
    LenderChoice { id: BALANCER_FLASH, lender: LeanLender::Balancer, extra_gas: BALANCER_EXTRA_GAS };

/// The choice a commitment's flash source names; `None` for any other source.
pub fn by_id(id: FlashProviderId) -> Option<LenderChoice> {
    [MORPHO, BALANCER].into_iter().find(|c| c.id == id)
}

/// What each lender holds of the loan token, as last read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lenders {
    pub balancer: U256,
    /// Morpho Blue's holding, once read.
    pub morpho: Option<U256>,
}

impl Lenders {
    /// The most any size may borrow.
    pub fn cap(&self) -> U256 {
        self.morpho.map_or(self.balancer, |m| m.max(self.balancer))
    }

    /// The cheapest lender holding at least `size`.
    pub fn for_size(&self, size: U256) -> Option<LenderChoice> {
        let mut covering: Vec<LenderChoice> = [(MORPHO, self.morpho), (BALANCER, Some(self.balancer))]
            .into_iter()
            .filter(|(_, held)| held.is_some_and(|h| size <= h))
            .map(|(c, _)| c)
            .collect();
        covering.sort_by_key(|c| c.extra_gas);
        covering.first().copied()
    }

    /// The gas `size`'s loan adds over the model's.
    pub fn extra_gas(&self, size: U256) -> u64 {
        self.for_size(size).map_or(0, |c| c.extra_gas)
    }
}
