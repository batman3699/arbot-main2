//! The flash lenders a cycle borrows from (Task 8.5 R27).
//!
//! - Balancer V2's vault: free, ~19 WETH on Base.
//! - Morpho Blue through `MorphoFlashLender` (ERC-3156): free, ~82,679 WETH
//!   (2026-10-13).
//!
//! A size borrows from Balancer while Balancer covers it (no wrapper hops),
//! and from Morpho above. Never from Aave: the deployed executor counts Aave's
//! later pull as profit (simulated 2026-10-13).

use crate::live::frontier::{BALANCER_FLASH, BALANCER_VAULT};
use alloy_primitives::{address, Address};
use apex_exec::commitment::{Loan, LoanProvider};
use apex_types::ids::FlashProviderId;
use ethers_core::types::U256;

/// The Morpho lender's loans: the next id after Balancer's.
pub const MORPHO_FLASH: FlashProviderId = FlashProviderId(2);

/// Morpho Blue on Base, whose holding is what the Morpho lender can lend.
pub const MORPHO_BLUE: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");

/// What a loan through `MorphoFlashLender` costs over Balancer's.
///
/// Measured on Base on 2026-10-13, through the deployed executor, with the
/// deployed lender's code (Morpho approved), on ticket 51's cycle at 15 WETH
/// (block 52,526,060): Balancer 461,224 gas, Morpho 582,924, so 121,700 more,
/// rounded up here.
///
/// The figure follows the lender's and executor's WETH balances: a balance
/// written from zero costs ~20k gas more than one changed from non-zero, and
/// the Morpho path's refunds are capped. With 1 wei of WETH left in each it
/// was 82,900. The first injected runs, at an address holding dust, read
/// 94,167.
pub const MORPHO_EXTRA_GAS: u64 = 122_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LenderChoice {
    pub id: FlashProviderId,
    pub provider: LoanProvider,
    /// The address the plan's loan names.
    pub address: Address,
    /// The gas this lender adds over Balancer's.
    pub extra_gas: u64,
}

pub const BALANCER: LenderChoice =
    LenderChoice { id: BALANCER_FLASH, provider: LoanProvider::Balancer, address: BALANCER_VAULT, extra_gas: 0 };

/// The Morpho lender deployed at `lender`.
pub const fn morpho(lender: Address) -> LenderChoice {
    LenderChoice { id: MORPHO_FLASH, provider: LoanProvider::Erc3156, address: lender, extra_gas: MORPHO_EXTRA_GAS }
}

/// What each lender holds of the loan token, as last read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lenders {
    pub balancer: U256,
    /// The Morpho lender's address and Morpho Blue's holding, when configured.
    pub morpho: Option<(Address, U256)>,
}

impl Lenders {
    pub const fn balancer_only(holding: U256) -> Self {
        Self { balancer: holding, morpho: None }
    }

    /// The most any size may borrow.
    pub fn cap(&self) -> U256 {
        self.morpho.map_or(self.balancer, |(_, m)| m.max(self.balancer))
    }

    /// The lender `size` borrows from: Balancer while it covers it, then Morpho.
    pub fn for_size(&self, size: U256) -> Option<LenderChoice> {
        if size <= self.balancer {
            return Some(BALANCER);
        }
        self.morpho.filter(|(_, m)| size <= *m).map(|(a, _)| morpho(a))
    }

    /// The gas `size`'s loan adds over Balancer's.
    pub fn extra_gas(&self, size: U256) -> u64 {
        self.for_size(size).map_or(0, |c| c.extra_gas)
    }
}

/// The executor's loan of `amount` of `token` from `choice`.
pub fn loan(choice: &LenderChoice, token: Address, amount: alloy_primitives::U256) -> Result<Loan, String> {
    match choice.provider {
        LoanProvider::Balancer | LoanProvider::Erc3156 => {
            Ok(Loan { token, amount, provider: choice.provider, provider_addr: choice.address })
        }
        other => Err(format!("the deployed executor cannot settle a {other:?} loan (R27)")),
    }
}
