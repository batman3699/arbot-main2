//! Task 3.1 — discrete sizing, and the three properties §14.3 demands of it.

use apex_econ::sizing::continuous::{optimize, ContinuousBudget, ContinuousOptimum};
use apex_econ::sizing::discrete::{refine, refine_detailed, RefineBudget};
use apex_math::engine::{CpmmEngine, CpmmState, ExactPricingEngine, Order};
use apex_math::finite_size::{SizedRoute, Surplus};
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

struct Route {
    sell: CpmmState,
    buy: CpmmState,
    gas: U256,
    cap: U256,
}

impl SizedRoute for Route {
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

fn route(spread_bps: u64, fee_bps: u32, gas_wei: u128) -> Route {
    let weth = U256::from(1_000_000_000_000_000_000_000u128);
    let usdc = U256::from(2_000_000_000_000u128);
    Route {
        sell: CpmmState {
            pool: token(10),
            token0: WETH(),
            token1: USDC(),
            reserve0: weth,
            reserve1: usdc * U256::from(10_000 + spread_bps) / U256::from(10_000u64),
            fee_bps,
        },
        buy: CpmmState {
            pool: token(11),
            token0: USDC(),
            token1: WETH(),
            reserve0: usdc,
            reserve1: weth,
            fee_bps,
        },
        gas: U256::from(gas_wei),
        cap: U256::from(50_000_000_000_000_000_000u128),
    }
}

const GAS: u128 = 20_000_000_000_000;

fn exact_net(r: &Route, x: U256) -> Surplus {
    let out = r.output(x).expect("priceable");
    let cost = x + r.fixed_cost();
    if out >= cost {
        Surplus::Gain(out - cost)
    } else {
        Surplus::Loss(cost - out)
    }
}

/// §14.3's core guarantee. Holds by construction — the climb starts at the
/// nearest integer and only moves to a strictly better point — and this checks
/// the implementation matches the construction.
#[test]
fn discrete_refinement_is_never_worse_than_the_continuous_optimum() {
    for spread in [65u64, 80, 200, 1000] {
        let r = route(spread, 30, GAS);
        let c = optimize(&r, ContinuousBudget::default()).expect("warm start");
        let d = refine_detailed(&r, c, RefineBudget::default()).expect("refined");

        let nearest = U256::from_dec_str(&format!("{:.0}", c.value().round())).expect("finite");
        let at_nearest = exact_net(&r, nearest.clamp(U256::one(), r.max_input()));
        assert!(
            d.net >= at_nearest,
            "spread {spread}: refinement regressed below its own starting point \
             ({:?} < {:?})",
            d.net,
            at_nearest
        );
    }
}

/// A route nothing can trade returns `None`, not a zero-size trade.
///
/// `Some(zero)` would be a size: it encodes to a real transaction, pays gas,
/// and moves nothing.
#[test]
fn no_profitable_size_returns_none_not_zero() {
    // 61 bps of spread against two 30 bps fees: the marginal rate clears parity
    // by 0.7 thousandths of a basis point, and no size covers a cent of gas.
    let r = route(61, 30, GAS);
    let c = optimize(&r, ContinuousBudget::default()).expect("warm start");
    assert_eq!(refine(&r, c, RefineBudget::default()), None);

    // ...and the detail is still reported, because "the best size loses 4 wei"
    // and "could not be priced" are different facts (§27).
    let detail = refine_detailed(&r, c, RefineBudget::default()).expect("evaluated");
    assert!(detail.size.is_none());
    assert!(!detail.net.is_gain());
    assert!(detail.amount > U256::zero(), "a size was still searched for");
}

/// A profitable route yields a size, and the size is the one that was measured.
#[test]
fn a_profitable_route_yields_the_size_that_was_evaluated() {
    let r = route(200, 30, GAS);
    let c = optimize(&r, ContinuousBudget::default()).expect("warm start");
    let detail = refine_detailed(&r, c, RefineBudget::default()).expect("evaluated");
    let size = detail.size.expect("profitable");

    // The minted size is exactly the amount the refinement evaluated -- not a
    // rounding of it, not the continuous optimum.
    assert_eq!(
        size.get(),
        apex_types::compat::u256_to_alloy(detail.amount),
        "the minted size must be the amount that was actually priced"
    );
    assert!(detail.net.is_gain());
    assert_eq!(exact_net(&r, detail.amount), detail.net);
}

/// The continuous stage is a warm start and is allowed to be wrong. Starting
/// the climb from a deliberately terrible suggestion must still reach a
/// profitable size.
#[test]
fn a_bad_warm_start_does_not_produce_a_bad_trade() {
    let r = route(200, 30, GAS);
    let good = optimize(&r, ContinuousBudget::default()).expect("warm start");
    let best = refine_detailed(&r, good, RefineBudget::default()).expect("refined");

    // `ContinuousOptimum` cannot be constructed outside its module, so the bad
    // start is produced by optimising over a pathologically narrow budget.
    let lazy = optimize(
        &r,
        ContinuousBudget {
            iterations: 0,
            ..ContinuousBudget::default()
        },
    )
    .expect("warm start");
    let from_lazy = refine_detailed(&r, lazy, RefineBudget::default()).expect("refined");

    assert!(from_lazy.net.is_gain(), "a bad warm start must still find a trade");
    // It need not match the good start exactly -- the climb is bounded -- but it
    // must be in the same neighbourhood rather than orders of magnitude off.
    let (a, b) = (best.net.magnitude(), from_lazy.net.magnitude());
    assert!(
        b * U256::from(2u64) >= a,
        "recovery from a bad warm start lost more than half the profit: {b} vs {a}"
    );
}

/// The search is bounded (§29) and still answers.
#[test]
fn refinement_respects_its_evaluation_budget() {
    let r = route(200, 30, GAS);
    let c = optimize(&r, ContinuousBudget::default()).expect("warm start");
    let detail = refine_detailed(
        &r,
        c,
        RefineBudget {
            max_evaluations: 3,
            ..RefineBudget::default()
        },
    )
    .expect("evaluated");
    assert!(detail.evaluations <= 3, "budget exceeded: {}", detail.evaluations);
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/proptest-regressions/discrete.txt",
        ))),
        ..ProptestConfig::default()
    })]

    /// The guarantee over arbitrary routes rather than four hand-picked ones.
    #[test]
    fn refinement_never_regresses_below_its_warm_start(
        spread in 0u64..3_000,
        fee_bps in 1u32..300,
        gas in 0u128..1_000_000_000_000_000u128,
    ) {
        let r = route(spread, fee_bps, gas);
        let Some(c) = optimize(&r, ContinuousBudget::default()) else { return Ok(()) };
        let Some(d) = refine_detailed(&r, c, RefineBudget::default()) else { return Ok(()) };
        let nearest = U256::from_dec_str(&format!("{:.0}", c.value().round()))
            .unwrap_or(U256::one())
            .clamp(U256::one(), r.max_input());
        prop_assert!(d.net >= exact_net(&r, nearest));
    }

    /// A minted size is always profitable. The inverse of
    /// `no_profitable_size_returns_none_not_zero`, over the whole space: if a
    /// `DiscreteSize` exists, executing it nets a gain.
    #[test]
    fn a_minted_size_is_always_profitable(
        spread in 0u64..3_000,
        fee_bps in 1u32..300,
        gas in 0u128..1_000_000_000_000_000u128,
    ) {
        let r = route(spread, fee_bps, gas);
        let Some(c) = optimize(&r, ContinuousBudget::default()) else { return Ok(()) };
        let Some(d) = refine_detailed(&r, c, RefineBudget::default()) else { return Ok(()) };
        if d.size.is_some() {
            prop_assert!(d.net.is_gain());
            prop_assert!(d.amount > U256::zero());
            prop_assert_eq!(exact_net(&r, d.amount), d.net);
        }
    }
}

/// A `ContinuousOptimum` has no route to a `DiscreteSize`.
///
/// The compile-fail half is `tests/compile_fail/`; this is the runtime half,
/// asserting the type exposes only an `f64` and that the only path from one to
/// a size goes through `refine`.
#[test]
fn a_continuous_optimum_is_not_a_size() {
    let r = route(200, 30, GAS);
    let c: ContinuousOptimum = optimize(&r, ContinuousBudget::default()).expect("warm start");
    let _: f64 = c.value();
    // `c` cannot be turned into a DiscreteSize except by refining it, and
    // refining re-evaluates the route exactly.
    let size = refine(&r, c, RefineBudget::default()).expect("profitable");
    assert_ne!(
        size.get(),
        apex_types::compat::u256_to_alloy(
            U256::from_dec_str(&format!("{:.0}", c.value().round())).unwrap_or_default()
        ),
        "if these matched, the size would be a rounded continuous optimum rather \
         than an exactly evaluated one -- fixture is too easy, pick a route where \
         the climb actually moves"
    );
}
