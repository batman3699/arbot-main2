use ethers::types::{Address, U256};

use crate::math::mul_div;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlashLoanProvider {
    Balancer,
    AaveV3,
    Erc3156,
    Univ2Flashswap,
    Univ3Flash,
}

impl FlashLoanProvider {
    pub fn as_id(self) -> u8 {
        match self {
            FlashLoanProvider::Balancer => 0u8,
            FlashLoanProvider::AaveV3 => 1u8,
            FlashLoanProvider::Erc3156 => 2u8,
            FlashLoanProvider::Univ2Flashswap => 3u8,
            FlashLoanProvider::Univ3Flash => 4u8,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlashLoanQuote {
    pub provider: FlashLoanProvider,
    pub max_amount: U256,
    pub fee_bps: u32,
    /// Provider contract address for PlanV2.providerAddr (vault/pool or ERC3156 lender).
    pub provider_addr: Option<Address>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlashLoanSelection {
    pub provider: FlashLoanProvider,
    pub amount: U256,
    pub fee_bps: u32,
    /// Provider contract address for PlanV2.providerAddr (vault/pool or ERC3156 lender).
    pub provider_addr: Option<Address>,
}

pub fn best_single_provider(amount: U256, quotes: &[FlashLoanQuote]) -> Option<FlashLoanSelection> {
    let mut candidates: Vec<&FlashLoanQuote> = quotes
        .iter()
        .filter(|quote| quote.max_amount >= amount && !amount.is_zero())
        .collect();

    candidates.sort_by(|a, b| {
        a.fee_bps
            .cmp(&b.fee_bps)
            .then_with(|| {
                provider_reliability_rank(a.provider).cmp(&provider_reliability_rank(b.provider))
            })
            .then_with(|| b.max_amount.cmp(&a.max_amount))
    });

    candidates
        .into_iter()
        .next()
        .map(|quote| FlashLoanSelection {
            provider: quote.provider,
            amount,
            fee_bps: quote.fee_bps,
            provider_addr: quote.provider_addr,
        })
}

fn provider_reliability_rank(provider: FlashLoanProvider) -> u8 {
    match provider {
        FlashLoanProvider::Balancer => 0,
        FlashLoanProvider::AaveV3 => 1,
        FlashLoanProvider::Erc3156 => 2,
        FlashLoanProvider::Univ3Flash => 3,
        FlashLoanProvider::Univ2Flashswap => 4,
    }
}

#[allow(dead_code)]
pub fn allocate_flash_loans(amount: U256, quotes: &[FlashLoanQuote]) -> Vec<FlashLoanSelection> {
    if amount.is_zero() {
        return Vec::new();
    }

    // PlanV2 execution currently supports a single flash loan. Multi-provider allocation is
    // intentionally disabled until multi-loan execution is implemented in the executor.
    best_single_provider(amount, quotes)
        .map(|selection| vec![selection])
        .unwrap_or_default()
}

pub fn flash_fee(amount: U256, fee_bps: u32) -> U256 {
    if amount.is_zero() || fee_bps == 0 {
        return U256::zero();
    }
    mul_div(amount, U256::from(fee_bps as u64), U256::from(10_000u64))
}

pub fn flash_fee_for_provider(provider: FlashLoanProvider, amount: U256, fee_bps: u32) -> U256 {
    match provider {
        FlashLoanProvider::Univ2Flashswap => univ2_flash_fee(amount),
        _ => flash_fee(amount, fee_bps),
    }
}

pub fn univ2_flash_fee(amount: U256) -> U256 {
    if amount.is_zero() {
        return U256::zero();
    }
    let repay =
        mul_div(amount, U256::from(1000u64), U256::from(997u64)).saturating_add(U256::one());
    repay.saturating_sub(amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_best_provider_by_fee_then_capacity() {
        let quotes = vec![
            FlashLoanQuote {
                provider: FlashLoanProvider::Balancer,
                max_amount: U256::from(1_000_000u64),
                fee_bps: 0,
                provider_addr: None,
            },
            FlashLoanQuote {
                provider: FlashLoanProvider::AaveV3,
                max_amount: U256::from(2_000_000u64),
                fee_bps: 9,
                provider_addr: None,
            },
        ];

        let selected = best_single_provider(U256::from(900_000u64), &quotes).unwrap();
        assert_eq!(selected.provider, FlashLoanProvider::Balancer);
        assert_eq!(selected.amount, U256::from(900_000u64));

        let selected_large = best_single_provider(U256::from(1_500_000u64), &quotes).unwrap();
        assert_eq!(selected_large.provider, FlashLoanProvider::AaveV3);
    }

    #[test]
    fn rejects_split_allocations_until_multi_loan_execution() {
        let quotes = vec![
            FlashLoanQuote {
                provider: FlashLoanProvider::Balancer,
                max_amount: U256::from(1_000u64),
                fee_bps: 0,
                provider_addr: None,
            },
            FlashLoanQuote {
                provider: FlashLoanProvider::AaveV3,
                max_amount: U256::from(1_000u64),
                fee_bps: 9,
                provider_addr: None,
            },
        ];

        let allocations = allocate_flash_loans(U256::from(1_500u64), &quotes);
        assert!(allocations.is_empty());
    }

    #[test]
    fn computes_flash_fee() {
        let amount = U256::from(1_000_000u64);
        let fee = flash_fee(amount, 9);
        assert_eq!(fee, U256::from(900u64));
    }

    #[test]
    fn computes_univ2_flash_fee() {
        let amount = U256::from(1_000_000u64);
        let fee = univ2_flash_fee(amount);
        assert_eq!(fee, U256::from(3_010u64));
    }
}
