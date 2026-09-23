//! Task 2.5 — Engine C, and the fixture the plan asked for that does not exist.

use apex_math::engine::{CpmmEngine, CpmmState, ExactPricingEngine, Order};
use apex_math::finite_size::{
    best_size, is_profitable_at, marginal_gross_bps, NoSize, SearchBudget, SizedRoute, Surplus,
};
use ethers_core::types::{Address, U256};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

fn token(n: u8) -> Address {
    let mut b = [0u8; 20];
    b[19] = n;
    Address::from(b)
}

const WETH: fn() -> Address = || token(1);
const USDC: fn() -> Address = || token(2);

/// WETH -> USDC on one pool, USDC -> WETH on another. The pairwise cross-venue
/// mismatch §12.2 names as Engine C's first mode.
struct TwoVenueCycle {
    sell: CpmmState,
    buy: CpmmState,
    gas: U256,
    cap: U256,
}

impl SizedRoute for TwoVenueCycle {
    fn output(&self, amount_in: U256) -> Option<U256> {
        let usdc = CpmmEngine
            .quote_exact(&self.sell, &Order::new(WETH(), amount_in))
            .ok()?
            .amount_out;
        Some(
            CpmmEngine
                .quote_exact(&self.buy, &Order::new(USDC(), usdc))
                .ok()?
                .amount_out,
        )
    }
    fn fixed_cost(&self) -> U256 {
        self.gas
    }
    fn max_input(&self) -> U256 {
        self.cap
    }
}

/// `spread_bps` is how much dearer WETH is on the selling pool than on the
/// buying one. The round trip pays both fees, so it only clears at a spread
/// above roughly twice the fee.
fn cycle(spread_bps: u64, fee_bps: u32, gas_wei: u128) -> TwoVenueCycle {
    let weth_depth = U256::from(1_000_000_000_000_000_000_000u128); // 1000 WETH
    let usdc_at_2000 = U256::from(2_000_000_000_000u128); // 2,000,000 USDC (6dp)
    let dearer = usdc_at_2000 * U256::from(10_000 + spread_bps) / U256::from(10_000u64);
    TwoVenueCycle {
        sell: CpmmState {
            pool: token(10),
            token0: WETH(),
            token1: USDC(),
            reserve0: weth_depth,
            reserve1: dearer,
            fee_bps,
        },
        buy: CpmmState {
            pool: token(11),
            token0: USDC(),
            token1: WETH(),
            reserve0: usdc_at_2000,
            reserve1: weth_depth,
            fee_bps,
        },
        gas: U256::from(gas_wei),
        cap: U256::from(50_000_000_000_000_000_000u128), // 50 WETH
    }
}

/// ~1 cent of gas at Base prices — this repository's own measured figure.
const GAS: u128 = 20_000_000_000_000;

/// 1e-3 WETH. Small enough to stand in for the marginal rate, large enough
/// that the 6-decimal middle leg does not truncate the answer away. See
/// `a_finite_size_never_beats_the_marginal_rate_on_gross`.
const PROBE_WEI: u128 = 1_000_000_000_000_000;
#[allow(non_snake_case)]
fn PROBE() -> U256 {
    U256::from(PROBE_WEI)
}

/// The fixture Task 2.5 asked for cannot be built, and this says why.
///
/// Every venue here prices with an output that is concave in its input and zero
/// at zero, so the average rate over `[0, x]` never exceeds the marginal rate
/// at 0. A cycle's finite-size gross is the product of its hops' average rates,
/// so it can never exceed the marginal gross. "Infinitesimal rates show no
/// negative cycle but a finite size is profitable" describes a convex market.
/// # The probe cannot be dust
///
/// Measured here first: at 1e-6 WETH the round trip reads **-21 bps** on a
/// fixture whose true marginal rate is -5, because the intermediate leg is
/// 6-decimal USDC and `apply_swap_fee`'s integer division throws away a
/// meaningful fraction of 2,000 raw units. A dust probe measures rounding, not
/// a rate — and it errs pessimistic, which is the safe direction but makes the
/// marginal rate unmeasurable that way. 1e-3 WETH puts ~2,000,000 raw units
/// through the middle leg and truncation falls below 0.01 bps.
#[test]
fn a_finite_size_never_beats_the_marginal_rate_on_gross() {
    let probe = PROBE();
    for spread in [50u64, 61, 65, 80, 200, 1000] {
        let c = cycle(spread, 30, 0); // gas zero: this is about GROSS
        let marginal = marginal_gross_bps(&c, probe).expect("priceable");
        for size in [1u128, 5, 12, 49, 120, 490, 3_440] {
            let x = U256::from(size) * U256::from(10_000_000_000_000_000u128); // size/100 WETH
            if x > c.max_input() || x < probe {
                continue;
            }
            let avg = marginal_gross_bps(&c, x).expect("priceable");
            assert!(
                avg <= marginal,
                "spread {spread} bps: average rate at {x} was {avg} bps, above the \
                 marginal {marginal} bps -- the curve is not concave"
            );
        }
    }
}

/// The band Engine A cannot express: marginal gross above parity, and no size
/// that pays for the transaction.
///
/// Engine A's weights are `-ln(rate)` with no cost term, so it calls every one
/// of these a negative cycle. Three of the five cannot be traded at any size.
#[test]
fn engine_c_refuses_cycles_engine_a_proposes() {
    let probe = PROBE();
    let budget = SearchBudget::default();

    // (spread, does any size clear ~1 cent of gas)
    let cases = [(61u64, false), (63, false), (65, true), (80, true), (200, true)];
    let mut refused = 0;
    for (spread, tradeable) in cases {
        let c = cycle(spread, 30, GAS);
        // Engine A's own question, not a rounded report of it: at 61 bps the
        // gross is +0.7 thousandths of a basis point, which `marginal_gross_bps`
        // renders as 0. Asserting on the rounded figure would have tested the
        // reporting helper rather than the engines.
        assert_eq!(
            is_profitable_at(&c, probe),
            Some(true),
            "spread {spread} bps must look profitable to Engine A"
        );

        match best_size(&c, budget) {
            Ok(found) => {
                assert!(tradeable, "spread {spread} bps should not have cleared gas");
                assert!(found.net.is_gain());
                assert!(found.amount_in > U256::zero());
            }
            Err(NoSize::NoProfitableSize { best }) => {
                assert!(
                    !tradeable,
                    "spread {spread} bps should have cleared gas; best was {best:?}"
                );
                refused += 1;
            }
            Err(other) => panic!("spread {spread} bps: unexpected {other:?}"),
        }
    }
    assert_eq!(refused, 2, "two of the five must be refused for cost");
}

/// And it says what size to trade, which a rate-only search cannot express.
#[test]
fn engine_c_reports_a_size_not_just_a_verdict() {
    let found = best_size(&cycle(200, 30, GAS), SearchBudget::default()).expect("profitable");
    // The optimum on this fixture sits in whole-WETH territory, far from both
    // the dust floor and the capacity ceiling — so the number is the curve's
    // answer, not a clamp.
    assert!(
        found.amount_in > U256::from(1_000_000_000_000_000_000u128),
        "optimum {} is suspiciously small",
        found.amount_in
    );
    assert!(found.amount_in < U256::from(20_000_000_000_000_000_000u128));

    // Moving away from the optimum in either direction must not improve net.
    let c = cycle(200, 30, GAS);
    for delta in [
        U256::from(100_000_000_000_000_000u128),
        U256::from(1_000_000_000_000_000_000u128),
    ] {
        for probe in [found.amount_in + delta, found.amount_in.saturating_sub(delta)] {
            if probe.is_zero() || probe > c.max_input() {
                continue;
            }
            let out = c.output(probe).expect("priceable");
            let net = if out >= probe + U256::from(GAS) {
                Surplus::Gain(out - probe - U256::from(GAS))
            } else {
                Surplus::Loss(probe + U256::from(GAS) - out)
            };
            assert!(
                net <= found.net,
                "size {probe} beat the reported optimum {}",
                found.amount_in
            );
        }
    }
}

/// A route with no usable range says so rather than returning a zero-size
/// "opportunity".
#[test]
fn an_empty_range_is_a_refusal_not_a_zero_size_trade() {
    let mut c = cycle(200, 30, GAS);
    c.cap = U256::from(1u64);
    assert_eq!(best_size(&c, SearchBudget::default()), Err(NoSize::RangeEmpty));
}

/// The search is bounded (§29). A budget of one evaluation still terminates
/// and still answers.
#[test]
fn the_search_respects_its_compute_budget() {
    let budget = SearchBudget {
        max_evaluations: 1,
        ..SearchBudget::default()
    };
    // One probe cannot find the optimum, but it must not hang and must not
    // claim a gain it did not measure.
    match best_size(&cycle(200, 30, GAS), budget) {
        Ok(found) => assert!(found.net.is_gain()),
        Err(NoSize::NoProfitableSize { .. }) => {}
        Err(other) => panic!("unexpected {other:?}"),
    }
}

/// `Surplus` ranks a loss below every gain, and a smaller loss above a larger
/// one, so `max` picks the best outcome even when nothing is profitable.
#[test]
fn surplus_orders_losses_the_right_way_round() {
    let one = U256::one();
    let two = U256::from(2u64);
    assert!(Surplus::Gain(one) > Surplus::Loss(one));
    assert!(Surplus::Gain(two) > Surplus::Gain(one));
    assert!(Surplus::Loss(one) > Surplus::Loss(two), "a smaller loss is better");
    let worst_to_best = [Surplus::Loss(two), Surplus::Loss(one), Surplus::Gain(one)];
    assert_eq!(
        worst_to_best.iter().max(),
        Some(&Surplus::Gain(one)),
        "max must find the gain"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/proptest-regressions/finite_size.txt",
        ))),
        ..ProptestConfig::default()
    })]

    /// Concavity, over arbitrary fixtures rather than the six hand-picked ones.
    ///
    /// If this ever fails, the premise underneath Engine C's whole design has
    /// changed and `a_finite_size_never_beats_the_marginal_rate_on_gross` is
    /// wrong -- which would mean some venue here prices convexly, and a great
    /// deal more than this test would need rethinking.
    #[test]
    fn the_average_rate_never_exceeds_the_marginal_rate(
        spread in 0u64..2_000,
        fee_bps in 1u32..300,
        size in PROBE_WEI..40_000_000_000_000_000_000u128,
    ) {
        let c = cycle(spread, fee_bps, 0);
        let Some(marginal) = marginal_gross_bps(&c, PROBE()) else { return Ok(()) };
        let Some(avg) = marginal_gross_bps(&c, U256::from(size)) else { return Ok(()) };
        prop_assert!(
            avg <= marginal,
            "spread {} fee {} size {}: average {} bps exceeds marginal {} bps",
            spread, fee_bps, size, avg, marginal
        );
    }
}
