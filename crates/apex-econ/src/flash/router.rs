//! Choosing where to borrow (§19, §19.4).
//!
//! §1.2: *a flash loan is financing, not edge.* The router prices that
//! financing. It does not make a losing route profitable and it is not allowed
//! to try.
//!
//! # The objective, and the input people forget
//!
//! §19 gives the selection rule as
//! `argmin(premium + gas_overhead·gasPrice + failure_risk_cost + availability_penalty)`.
//! Three of those four terms are properties of the provider. The fourth is
//! not.
//!
//! An availability penalty is `P(unavailable) × (what the outage costs)`, and
//! what an outage costs is the opportunity it loses — which belongs to the
//! trade, not to the lender. **A provider that is 1% flaky is fine for a trade
//! worth a cent and unacceptable for one worth ten thousand dollars.** So the
//! ranking cannot be computed from a quote alone, and [`select`] takes the
//! opportunity's value as an argument rather than pretending otherwise.
//!
//! # Probabilities cross into wei exactly once, and pessimistically
//!
//! `availability_probability` and `reliability_score` are `f64`; every cost
//! here is wei. The conversion happens once, at [`to_ppm`], and rounds **down**
//! — treating a provider as slightly less available than claimed. Rounding the
//! other way would shave the penalty on exactly the providers whose
//! availability is least certain.

use apex_types::flash::FlashSourceQuote;
use apex_types::ids::FlashProviderId;
use ethers_core::types::U256;

/// What the trade needs, and what it is worth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BorrowContext {
    /// How much must be borrowed. A provider that cannot supply it is excluded
    /// rather than scored, preserving the legacy capacity bound.
    pub required_amount: U256,
    pub gas_price_wei: U256,
    /// What is lost if this provider is unavailable when the moment comes:
    /// the opportunity's own net value. Zero is legitimate — it says an outage
    /// costs nothing, which is true when a fallback is free.
    pub opportunity_value_wei: U256,
    /// Gas already spent when a mid-execution failure occurs. Distinct from
    /// `gas_overhead`, which is what a SUCCESSFUL borrow costs.
    pub gas_at_risk: u64,
}

/// Why a provider was not considered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Excluded {
    /// Measured capacity below the required amount.
    InsufficientCapacity { available: U256, required: U256 },
    /// Availability is zero: the provider is down, not merely unreliable.
    Unavailable,
    /// The asset is not the one the trade needs.
    WrongAsset,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scored<'a> {
    pub quote: &'a FlashSourceQuote,
    pub premium: U256,
    pub gas_cost: U256,
    pub failure_risk_cost: U256,
    pub availability_penalty: U256,
    pub total: U256,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Selection<'a> {
    pub chosen: Scored<'a>,
    /// Every provider that was not chosen, with the reason. §19.4's outage
    /// requirement is only checkable if the exclusions are visible.
    pub excluded: Vec<(FlashProviderId, Excluded)>,
    /// The runner-up's total, when there was one.
    pub next_best_total: Option<U256>,
}

/// A probability in `[0, 1]` as integer parts per million, rounded DOWN.
///
/// Anything outside the unit interval is clamped, and NaN becomes zero — an
/// unparseable availability is treated as "never available", which excludes the
/// provider rather than scoring it favourably.
pub fn to_ppm(probability: f64) -> u32 {
    if !probability.is_finite() || probability <= 0.0 {
        return 0;
    }
    ((probability.min(1.0)) * 1_000_000.0).floor() as u32
}

fn score<'a>(quote: &'a FlashSourceQuote, ctx: BorrowContext) -> Scored<'a> {
    let premium = apex_types::compat::u256_to_ethers(quote.premium);
    let gas_cost = U256::from(quote.gas_overhead).saturating_mul(ctx.gas_price_wei);

    // What a mid-execution failure costs: the gas already burned, weighted by
    // how unreliable the provider is. `reliability_score` is the probability
    // it behaves; `1 - it` is the probability it does not.
    let unreliable_ppm = 1_000_000u32.saturating_sub(to_ppm(quote.reliability_score));
    let failure_risk_cost = U256::from(ctx.gas_at_risk)
        .saturating_mul(ctx.gas_price_wei)
        .saturating_mul(U256::from(unreliable_ppm))
        / U256::from(1_000_000u64);

    // What an outage costs: the opportunity, weighted by how often it is down.
    let unavailable_ppm = 1_000_000u32.saturating_sub(to_ppm(quote.availability_probability));
    let availability_penalty = ctx
        .opportunity_value_wei
        .saturating_mul(U256::from(unavailable_ppm))
        / U256::from(1_000_000u64);

    Scored {
        quote,
        premium,
        gas_cost,
        failure_risk_cost,
        availability_penalty,
        total: premium
            .saturating_add(gas_cost)
            .saturating_add(failure_risk_cost)
            .saturating_add(availability_penalty),
    }
}

fn exclusion(quote: &FlashSourceQuote, ctx: BorrowContext) -> Option<Excluded> {
    let available = apex_types::compat::u256_to_ethers(quote.amount);
    if available < ctx.required_amount {
        return Some(Excluded::InsufficientCapacity {
            available,
            required: ctx.required_amount,
        });
    }
    if to_ppm(quote.availability_probability) == 0 {
        return Some(Excluded::Unavailable);
    }
    None
}

/// Pick the cheapest source of financing, or say that none can serve the
/// trade.
///
/// §19.4: **a single provider's outage must not block selection.** An excluded
/// provider is removed from the ranking and recorded; the others are still
/// scored. `None` means every provider was excluded, which is a different fact
/// from "the cheapest one is expensive" and is reported as such by the empty
/// return rather than by a sentinel score.
pub fn select<'a>(quotes: &'a [FlashSourceQuote], ctx: BorrowContext) -> Option<Selection<'a>> {
    let mut excluded = Vec::new();
    let mut eligible: Vec<Scored<'a>> = Vec::new();
    for quote in quotes {
        match exclusion(quote, ctx) {
            Some(reason) => excluded.push((quote.provider, reason)),
            None => eligible.push(score(quote, ctx)),
        }
    }
    // Stable: equal totals keep the caller's order, so a tie is broken by the
    // caller's preference rather than by enumeration.
    eligible.sort_by_key(|s| s.total);
    let chosen = eligible.first()?.clone();
    let next_best_total = eligible.get(1).map(|s| s.total);
    Some(Selection {
        chosen,
        excluded,
        next_best_total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256 as AU256};
    use apex_types::flash::CallbackConstraints;
    use apex_types::ids::{ChainId, TokenId};

    fn token() -> TokenId {
        TokenId {
            chain: ChainId::BASE,
            address: Address::repeat_byte(1),
        }
    }

    fn quote(
        id: u16,
        amount: u128,
        premium: u128,
        gas_overhead: u64,
        availability: f64,
        reliability: f64,
    ) -> FlashSourceQuote {
        FlashSourceQuote {
            provider: FlashProviderId(id),
            asset: token(),
            amount: AU256::from(amount),
            premium: AU256::from(premium),
            gas_overhead,
            callback_constraints: CallbackConstraints {
                repay_by_transfer: true,
                reentrancy_permitted: false,
                max_callback_gas: 1_000_000,
            },
            availability_probability: availability,
            state_dependencies: Vec::new(),
            reliability_score: reliability,
        }
    }

    fn ctx() -> BorrowContext {
        BorrowContext {
            required_amount: U256::from(1_000_000_000_000_000_000u128),
            gas_price_wei: U256::from(10_000_000u64),
            opportunity_value_wei: U256::from(1_000_000_000_000_000u128), // 0.001 ETH
            gas_at_risk: 150_000,
        }
    }

    /// All nine §19.1 fields are present. Asserted by constructing one: a
    /// missing field is a compile error, and a field silently added would make
    /// this fail to build until someone decides what it means.
    #[test]
    fn a_flash_quote_carries_every_field_section_19_1_requires() {
        let q = quote(1, 10_000, 5, 120_000, 0.99, 0.999);
        assert_eq!(q.provider, FlashProviderId(1));
        assert_eq!(q.asset, token());
        assert!(q.amount > AU256::ZERO);
        assert!(q.premium > AU256::ZERO);
        assert!(q.gas_overhead > 0);
        assert!(!q.callback_constraints.reentrancy_permitted);
        assert!(q.availability_probability > 0.0);
        assert!(q.state_dependencies.is_empty());
        assert!(q.reliability_score > 0.0);
    }

    /// Capacity below the requirement excludes, it does not merely penalise.
    #[test]
    fn a_provider_that_cannot_supply_the_amount_is_excluded() {
        let quotes = [
            quote(1, 100, 0, 0, 1.0, 1.0), // far too small, and otherwise free
            quote(2, 2_000_000_000_000_000_000, 1_000_000, 120_000, 1.0, 1.0),
        ];
        let s = select(&quotes, ctx()).expect("one provider can serve");
        assert_eq!(s.chosen.quote.provider, FlashProviderId(2));
        assert_eq!(
            s.excluded,
            vec![(
                FlashProviderId(1),
                Excluded::InsufficientCapacity {
                    available: U256::from(100u64),
                    required: ctx().required_amount,
                }
            )],
            "the cheapest provider must be excluded on capacity, not out-scored"
        );
    }

    /// §19.4. One provider down must not stop the router answering.
    #[test]
    fn a_single_provider_outage_does_not_block_selection() {
        let big = 2_000_000_000_000_000_000u128;
        let quotes = [
            quote(1, big, 1, 0, 0.0, 1.0), // down, and would otherwise win
            quote(2, big, 1_000_000, 120_000, 1.0, 1.0),
            quote(3, big, 2_000_000, 120_000, 1.0, 1.0),
        ];
        let s = select(&quotes, ctx()).expect("two providers remain");
        assert_eq!(s.chosen.quote.provider, FlashProviderId(2));
        assert_eq!(s.excluded, vec![(FlashProviderId(1), Excluded::Unavailable)]);
        assert!(s.next_best_total.is_some(), "the third provider is still ranked");
    }

    /// Every provider excluded is a different fact from "the best is
    /// expensive", and is reported by returning nothing.
    #[test]
    fn no_provider_can_serve_returns_nothing() {
        let quotes = [quote(1, 100, 0, 0, 1.0, 1.0), quote(2, 200, 0, 0, 1.0, 1.0)];
        assert!(select(&quotes, ctx()).is_none());
    }

    /// The objective is the sum §19 specifies, term by term.
    #[test]
    fn the_score_is_the_sum_of_the_four_terms() {
        let big = 2_000_000_000_000_000_000u128;
        let q = quote(1, big, 500_000, 120_000, 0.99, 0.995);
        let s = select(std::slice::from_ref(&q), ctx()).expect("eligible");
        let c = s.chosen;
        assert_eq!(c.premium, U256::from(500_000u64));
        assert_eq!(c.gas_cost, U256::from(120_000u64) * ctx().gas_price_wei);
        // 0.5% unreliable on 150,000 gas at 0.01 gwei.
        assert_eq!(
            c.failure_risk_cost,
            U256::from(150_000u64) * ctx().gas_price_wei * U256::from(5_000u64)
                / U256::from(1_000_000u64)
        );
        // 1% of the opportunity.
        assert_eq!(
            c.availability_penalty,
            ctx().opportunity_value_wei * U256::from(10_000u64) / U256::from(1_000_000u64)
        );
        assert_eq!(
            c.total,
            c.premium + c.gas_cost + c.failure_risk_cost + c.availability_penalty
        );
    }

    /// The point the specification leaves implicit: the ranking DEPENDS on
    /// what the trade is worth. The same two providers swap places when the
    /// opportunity gets large enough for a 1% outage to matter.
    #[test]
    fn a_flaky_cheap_provider_loses_once_the_trade_is_big_enough() {
        // The premium gap is 0.002 ETH and the flaky provider is 2% down, so
        // the two are equal when the opportunity is worth 0.1 ETH. That
        // crossover is the quantity this test pins; the first fixture put the
        // gap at 1.9e6 wei, where a 2% penalty on any realistic opportunity
        // dwarfs it and the reliable provider wins everywhere.
        let big = 200_000_000_000_000_000_000u128;
        let quotes = [
            quote(1, big, 100_000_000_000_000, 120_000, 0.98, 1.0), // cheap, 2% down
            quote(2, big, 2_100_000_000_000_000, 120_000, 1.0, 1.0), // dear, never down
        ];

        let small_trade = BorrowContext {
            opportunity_value_wei: U256::from(10_000_000_000_000_000u128), // 0.01 ETH
            ..ctx()
        };
        assert_eq!(
            select(&quotes, small_trade).expect("eligible").chosen.quote.provider,
            FlashProviderId(1),
            "on a trivial trade the cheap provider wins"
        );

        let large_trade = BorrowContext {
            opportunity_value_wei: U256::from(1_000_000_000_000_000_000u128), // 1 ETH
            ..ctx()
        };
        assert_eq!(
            select(&quotes, large_trade).expect("eligible").chosen.quote.provider,
            FlashProviderId(2),
            "once 2% of the opportunity exceeds the premium gap, reliability wins"
        );

        // And the crossover is where the algebra says: premium gap / outage
        // rate = 0.002 / 0.02 = 0.1 ETH.
        let just_under = BorrowContext {
            opportunity_value_wei: U256::from(99_000_000_000_000_000u128),
            ..ctx()
        };
        let just_over = BorrowContext {
            opportunity_value_wei: U256::from(101_000_000_000_000_000u128),
            ..ctx()
        };
        assert_eq!(
            select(&quotes, just_under).expect("eligible").chosen.quote.provider,
            FlashProviderId(1)
        );
        assert_eq!(
            select(&quotes, just_over).expect("eligible").chosen.quote.provider,
            FlashProviderId(2)
        );
    }

    /// Probabilities round DOWN into ppm, so a provider is never scored as
    /// more available than it claims.
    #[test]
    fn availability_rounds_against_the_provider() {
        assert_eq!(to_ppm(0.999_999_9), 999_999, "rounded down, not up");
        assert_eq!(to_ppm(1.0), 1_000_000);
        assert_eq!(to_ppm(0.0), 0);
        assert_eq!(to_ppm(-0.5), 0);
        assert_eq!(to_ppm(2.0), 1_000_000, "clamped");
        assert_eq!(to_ppm(f64::NAN), 0, "an unparseable availability is never available");
    }

    /// A NaN availability excludes rather than scoring well.
    #[test]
    fn an_unparseable_availability_excludes_the_provider() {
        let big = 2_000_000_000_000_000_000u128;
        let quotes = [
            quote(1, big, 0, 0, f64::NAN, 1.0),
            quote(2, big, 1_000_000, 120_000, 1.0, 1.0),
        ];
        let s = select(&quotes, ctx()).expect("one remains");
        assert_eq!(s.chosen.quote.provider, FlashProviderId(2));
        assert_eq!(s.excluded, vec![(FlashProviderId(1), Excluded::Unavailable)]);
    }
}
