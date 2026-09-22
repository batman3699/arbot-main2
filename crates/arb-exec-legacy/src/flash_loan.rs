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

/// Default constant-product swap fee, in bps. UniV2's immutable 0.30%, and also
/// Aerodrome's current volatile-pool default — but Aerodrome's is factory-set
/// per pool, so callers that know the pool's real fee must pass it.
pub const DEFAULT_UNIV2_FLASH_FEE_BPS: u32 = 30;

pub fn flash_fee_for_provider(provider: FlashLoanProvider, amount: U256, fee_bps: u32) -> U256 {
    match provider {
        // A flash swap is repaid through the pool's own swap curve, so the cost
        // is the pool fee grossed up — not a flat percentage of the principal.
        FlashLoanProvider::Univ2Flashswap => univ2_flash_fee_bps(amount, fee_bps),
        _ => flash_fee(amount, fee_bps),
    }
}

/// Repayment premium for a constant-product flash swap.
///
/// Borrowing `amount` requires returning `amount * 10000 / (10000 - fee_bps)`,
/// rounded up. `fee_bps == 0` means "unspecified", not "free": a flash swap is
/// never free, so it falls back to the 0.30% default rather than pricing the
/// loan at zero, which would mark losing trades profitable.
pub fn univ2_flash_fee_bps(amount: U256, fee_bps: u32) -> U256 {
    if amount.is_zero() {
        return U256::zero();
    }
    let fee_bps = if fee_bps == 0 {
        DEFAULT_UNIV2_FLASH_FEE_BPS
    } else {
        fee_bps
    };
    // Guard a pathological config: a >=100% fee has no finite repayment.
    let denom = 10_000u64.saturating_sub(fee_bps.min(9_999) as u64);
    let repay = mul_div(amount, U256::from(10_000u64), U256::from(denom)).saturating_add(U256::one());
    repay.saturating_sub(amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flash_swap_is_never_free_even_when_fee_is_unset() {
        // `fee_bps: 0` on a flash-swap quote means "unspecified", not "free".
        // Pricing the loan at zero would make losing trades look profitable.
        let amount = U256::from(1_000_000_000u64);
        let fee = flash_fee_for_provider(FlashLoanProvider::Univ2Flashswap, amount, 0);
        assert!(!fee.is_zero(), "a flash swap always costs the pool fee");
        assert_eq!(
            fee,
            univ2_flash_fee_bps(amount, DEFAULT_UNIV2_FLASH_FEE_BPS),
            "unset must fall back to the 0.30% default"
        );
    }

    #[test]
    fn flash_swap_fee_tracks_the_pool_fee() {
        // Aerodrome sets its fee per pool via the factory, so a hardcoded 0.30%
        // would misprice any pool that is not at the default.
        let amount = U256::from(1_000_000_000u64);
        let cheap = univ2_flash_fee_bps(amount, 5);
        let default = univ2_flash_fee_bps(amount, 30);
        let dear = univ2_flash_fee_bps(amount, 100);
        assert!(cheap < default && default < dear, "fee must be monotone in bps");
        // 30bps on 1e9 => 1e9*10000/9970 - 1e9 ~= 3_009_027
        assert_eq!(default, U256::from(3_009_028u64));
    }

    #[test]
    fn flash_swap_fee_survives_a_pathological_fee() {
        // A >=100% fee has no finite repayment; it must clamp, not overflow.
        let amount = U256::from(1_000u64);
        assert!(!univ2_flash_fee_bps(amount, 10_000).is_zero());
        assert!(!univ2_flash_fee_bps(amount, 50_000).is_zero());
        assert!(univ2_flash_fee_bps(U256::zero(), 30).is_zero());
    }

    #[test]
    fn univ3_flash_is_priced_flat_on_principal() {
        // UniV3 flash charges the tier fee as a flat share of principal — and it
        // is NOT routed through the flash-swap curve.
        let amount = U256::from(1_000_000u64);
        assert_eq!(
            flash_fee_for_provider(FlashLoanProvider::Univ3Flash, amount, 30),
            U256::from(3_000u64)
        );
    }

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
        // Unchanged from the 997/1000 form: the default is still 0.30%.
        let amount = U256::from(1_000_000u64);
        let fee = univ2_flash_fee_bps(amount, DEFAULT_UNIV2_FLASH_FEE_BPS);
        assert_eq!(fee, U256::from(3_010u64));
    }
}
