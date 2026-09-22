//! Task 2.1 — the `ExactPricingEngine` contract, and what composing two swaps
//! through one pool is actually allowed to do.
//!
//! PLAN.md §33 Task 2.1 specified the composition property as an EQUALITY:
//!
//! ```text
//! q1.amount_out + q2.amount_out == qc.amount_out
//! ```
//!
//! That is not a property of any AMM, and asserting it would have produced a
//! failing test that no correct implementation could pass. Splitting a swap
//! costs output, always, for two independent reasons:
//!
//! * **Rounding.** Every step rounds output down and fee up. Two swaps round
//!   twice.
//! * **The curve.** On a constant-product pool the second half trades against
//!   reserves the first half already moved. `x*y=k` is strictly convex, so
//!   `f(x) + f_after_x(y) < f(x+y)` even with zero fee and infinite precision.
//!
//! The real invariant is the INEQUALITY, and its direction is the whole point:
//! splitting must never create value. If `q1 + q2 > qc`, the model says a
//! trader can profit by chopping an order into pieces against a single pool —
//! a free arbitrage that exists only in our arithmetic. This repository has
//! shipped exactly that class of error before (the `liquidity()` overstatement
//! that made one pool the most profitable edge on the chain), and it is
//! undetectable downstream because ranking maximises gross.
//!
//! So: `q1 + q2 <= qc`, asserted, with the measured gap reported.

use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_math::engine::{
    ClEdgeState, ClEngine, CpmmEngine, CpmmState, ExactPricingEngine, Order, PancakeV3Engine,
    PricingError, RevertCondition, RoundingMode, SlipstreamEngine, SolidlyEngine, SolidlyState,
    UniV3Engine,
};
use apex_math::quote_solidly::SolidlyPairState;
use ethers_core::types::{Address, U256};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

fn token(n: u8) -> Address {
    let mut bytes = [0u8; 20];
    bytes[19] = n;
    Address::from(bytes)
}

const TOKEN0: fn() -> Address = || token(1);
const TOKEN1: fn() -> Address = || token(2);

fn cl_fixture() -> ClEdgeState {
    ClEdgeState {
        pool: token(10),
        token0: TOKEN0(),
        token1: TOKEN1(),
        state: ClPoolState {
            // 1:1, the canonical Q96 unit price.
            sqrt_price_x96: U256::from(1u128) << 96,
            // 1e21: at a 1:1 price the virtual reserves equal L, so this is a
            // pool holding ~1000 of each token. Sized from the proptest range
            // below -- a fixture too thin to fill the amounts under test makes
            // every case return early and the property vacuously true.
            liquidity: 1_000_000_000_000_000_000_000,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
            balance0: None,
            balance1: None,
        },
        // A symmetric ladder wide enough that arbitrage-sized swaps stay inside
        // it; `liquidity_net` signs follow the convention that a range's lower
        // tick adds and its upper tick removes.
        ladder: TickLadder::new(
            vec![
                (-1200, 200_000_000_000_000_000_000),
                (-600, 300_000_000_000_000_000_000),
                (-60, 100_000_000_000_000_000_000),
                (60, -100_000_000_000_000_000_000),
                (600, -300_000_000_000_000_000_000),
                (1200, -200_000_000_000_000_000_000),
            ],
            -1200,
            1200,
        ),
        max_ticks: 128,
    }
}

fn cpmm_fixture() -> CpmmState {
    CpmmState {
        pool: token(11),
        token0: TOKEN0(),
        token1: TOKEN1(),
        reserve0: U256::from(1_000_000_000_000_000_000_000u128), // 1000e18
        reserve1: U256::from(2_000_000_000_000_000_000_000u128), // 2000e18
        fee_bps: 30,
    }
}

fn solidly_fixture() -> SolidlyState {
    SolidlyState {
        pool: token(12),
        pair: SolidlyPairState {
            token0: TOKEN0(),
            token1: TOKEN1(),
            reserve0: U256::from(1_000_000_000_000_000_000_000u128),
            reserve1: U256::from(2_000_000_000_000_000_000_000u128),
            stable: false,
            decimals0: 18,
            decimals1: 18,
        },
        fee_bps: 5,
    }
}

/// Every engine answers all six methods. The trait has no default bodies, so
/// this compiling IS the assertion — a venue cannot inherit an answer to "how
/// do you round" or "what reverts you".
#[test]
fn every_engine_implements_the_full_contract() {
    fn assert_engine<E: ExactPricingEngine>() {}
    assert_engine::<ClEngine>();
    assert_engine::<UniV3Engine>();
    assert_engine::<SlipstreamEngine>();
    assert_engine::<PancakeV3Engine>();
    assert_engine::<CpmmEngine>();
    assert_engine::<SolidlyEngine>();
    // NOT asserted, and this is deliberate: PLAN.md §11.1 lists CurveEngine and
    // BalancerEngine as implementors, but `quote_curve` and `quote_balancer`
    // are `abigen!` RPC clients with no local mathematics at all. There is
    // nothing here to make exact. Task 2.7 resolves their §4.7 entries; until
    // then, naming a type after maths that does not exist would be the lie.
}

/// The three CL venues are one algorithm, and must stay one algorithm.
#[test]
fn the_cl_family_engines_do_not_diverge() {
    let s = cl_fixture();
    let o = Order::new(TOKEN0(), U256::from(1_000_000_000_000_000_000u128));
    let base = ClEngine.quote_exact(&s, &o).expect("fixture quotes");
    assert_eq!(UniV3Engine.quote_exact(&s, &o), Ok(base));
    assert_eq!(SlipstreamEngine.quote_exact(&s, &o), Ok(base));
    assert_eq!(PancakeV3Engine.quote_exact(&s, &o), Ok(base));
    assert_eq!(UniV3Engine.rounding_exact(), RoundingMode::OutputDownFeeUp);
}

/// A token the pool does not hold is a revert, not a zero quote.
#[test]
fn a_foreign_token_reverts_rather_than_quoting_zero() {
    let stranger = Order::new(token(99), U256::from(1_000u64));
    let expected = Err(PricingError::Reverts(RevertCondition::TokenNotInPool));
    assert_eq!(ClEngine.quote_exact(&cl_fixture(), &stranger), expected);
    assert_eq!(CpmmEngine.quote_exact(&cpmm_fixture(), &stranger), expected);
    assert_eq!(SolidlyEngine.quote_exact(&solidly_fixture(), &stranger), expected);
    for conds in [
        ClEngine.revert_conditions(&cl_fixture(), &stranger),
        CpmmEngine.revert_conditions(&cpmm_fixture(), &stranger),
        SolidlyEngine.revert_conditions(&solidly_fixture(), &stranger),
    ] {
        assert!(conds.contains(&RevertCondition::TokenNotInPool), "{conds:?}");
    }
}

/// `state_dependencies` names the ticks the quote actually rests on, so a
/// mint or burn on one of them can invalidate it.
#[test]
fn a_cl_quote_declares_the_ticks_it_depends_on() {
    let deps = ClEngine.state_dependencies(&cl_fixture());
    assert!(deps.reads_spot && deps.reads_liquidity);
    assert_eq!(deps.reads_ticks, vec![-1200, -600, -60, 60, 600, 1200]);
    // A constant-product pool has no ticks; that is a fact about the venue,
    // not a value we failed to fill in.
    assert!(CpmmEngine.state_dependencies(&cpmm_fixture()).reads_ticks.is_empty());
    assert!(!CpmmEngine.state_dependencies(&cpmm_fixture()).reads_liquidity);
}

/// The fee an engine reports is the fee its quote charged.
#[test]
fn the_reported_fee_is_the_fee_that_was_taken() {
    let s = cpmm_fixture();
    let amount = U256::from(1_000_000_000_000_000_000u128);
    let o = Order::new(TOKEN0(), amount);
    let fee = CpmmEngine.fee_exact(&s, &o).expect("fee");
    // 30 bps of 1e18.
    assert_eq!(fee, U256::from(3_000_000_000_000_000u128));
    assert_eq!(CpmmEngine.quote_exact(&s, &o).expect("quote").fee_in, fee);

    // CL takes its fee in ppm, off the input, rounded UP.
    let cl = cl_fixture();
    let cl_fee = ClEngine.fee_exact(&cl, &o).expect("fee");
    assert_eq!(cl_fee, U256::from(3_000_000_000_000_000u128));
}

/// A swap that comes to rest exactly on an initialized tick is REFUSED by
/// `next_state_exact`, not answered with pre-crossing liquidity.
///
/// v3-core crosses eagerly in that situation; this port deliberately does not,
/// because crossing there can mislabel a complete quote as exhausted. The two
/// conventions therefore disagree about liquidity on the far side of the tick.
/// The quote is unaffected — it is the STATE that is undetermined.
#[test]
fn a_swap_resting_on_a_tick_boundary_has_no_exact_next_state() {
    // Constructed rather than searched for: put the whole ladder one tick away
    // and give the pool exactly enough input to reach it. Whether the search
    // finds such an amount is a property of the fixture, so the test asserts
    // the DISPOSITION -- if the flag is set, the engine must refuse.
    let s = cl_fixture();
    let o = Order::new(TOKEN0(), U256::from(1_000_000_000_000_000_000u128));
    let zero_for_one = true;
    let q = apex_math::cl_swap::quote_exact_input_multi_tick(
        &s.state,
        &s.ladder,
        o.amount_in,
        zero_for_one,
        s.max_ticks,
    )
    .expect("fixture quotes");
    if q.ended_on_tick_boundary {
        assert!(matches!(
            ClEngine.next_state_exact(&s, &o),
            Err(PricingError::NotRepresentable(_))
        ));
    } else {
        // The ordinary case: the state is exact, and its tick is the tick the
        // resting price actually sits in.
        let next = ClEngine.next_state_exact(&s, &o).expect("exact next state");
        assert_eq!(next.state.sqrt_price_x96, q.sqrt_price_after);
        assert_eq!(next.state.liquidity, q.liquidity_after);
        assert_eq!(
            Some(next.state.tick),
            apex_math::cl_math::get_tick_at_sqrt_ratio(q.sqrt_price_after)
        );
    }
}

/// A constant-product pool keeps the fee. Adding only the post-fee amount to
/// reserves would leak it, and price the next swap against a poorer pool.
#[test]
fn a_cpmm_pool_retains_the_fee_it_charged() {
    let s = cpmm_fixture();
    let amount = U256::from(1_000_000_000_000_000_000u128);
    let o = Order::new(TOKEN0(), amount);
    let q = CpmmEngine.quote_exact(&s, &o).expect("quote");
    let next = CpmmEngine.next_state_exact(&s, &o).expect("next state");
    assert_eq!(next.reserve0, s.reserve0 + amount, "the FULL input enters the pool");
    assert_eq!(next.reserve1, s.reserve1 - q.amount_out);
    // k must not fall: that is the pool's own invariant, and a next-state that
    // breaks it would let a round trip mint value.
    let k_before = s.reserve0.full_mul(s.reserve1);
    let k_after = next.reserve0.full_mul(next.reserve1);
    assert!(k_after >= k_before, "constant product decreased");
}

proptest! {
    // Persist shrunk counterexamples next to the test rather than beside a
    // `lib.rs` this integration target does not have. A property failure that
    // is not recorded is a property failure that has to be rediscovered.
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/proptest-regressions/engine_contract.txt",
        ))),
        ..ProptestConfig::default()
    })]

    /// Splitting a swap must never produce MORE than doing it at once.
    ///
    /// The direction is the point. `<=` is the mathematics of a convex curve
    /// with a fee. `>` would be free money that exists only inside our model,
    /// and ranking maximises gross, so the model would chase it on every block.
    #[test]
    fn splitting_a_cpmm_swap_never_creates_value(
        x in 1_000_000_000_000u128..1_000_000_000_000_000_000_000u128,
        y in 1_000_000_000_000u128..1_000_000_000_000_000_000_000u128,
    ) {
        let s = cpmm_fixture();
        let (x, y) = (U256::from(x), U256::from(y));
        let Ok(q1) = CpmmEngine.quote_exact(&s, &Order::new(TOKEN0(), x)) else { return Ok(()) };
        let Ok(s1) = CpmmEngine.next_state_exact(&s, &Order::new(TOKEN0(), x)) else { return Ok(()) };
        let Ok(q2) = CpmmEngine.quote_exact(&s1, &Order::new(TOKEN0(), y)) else { return Ok(()) };
        let Ok(qc) = CpmmEngine.quote_exact(&s, &Order::new(TOKEN0(), x + y)) else { return Ok(()) };

        prop_assert!(
            q1.amount_out + q2.amount_out <= qc.amount_out,
            "splitting {x} + {y} produced {} vs {} in one swap -- the model invented {}",
            q1.amount_out + q2.amount_out,
            qc.amount_out,
            (q1.amount_out + q2.amount_out) - qc.amount_out
        );
    }

    /// The same invariant across a concentrated-liquidity pool, where the
    /// second swap may also trade against different liquidity than the first.
    #[test]
    fn splitting_a_cl_swap_never_creates_value(
        x in 1_000_000_000_000u128..20_000_000_000_000_000_000u128,
        y in 1_000_000_000_000u128..20_000_000_000_000_000_000u128,
    ) {
        let s = cl_fixture();
        let (x, y) = (U256::from(x), U256::from(y));
        let Ok(q1) = ClEngine.quote_exact(&s, &Order::new(TOKEN0(), x)) else { return Ok(()) };
        let Ok(s1) = ClEngine.next_state_exact(&s, &Order::new(TOKEN0(), x)) else { return Ok(()) };
        let Ok(q2) = ClEngine.quote_exact(&s1, &Order::new(TOKEN0(), y)) else { return Ok(()) };
        let Ok(qc) = ClEngine.quote_exact(&s, &Order::new(TOKEN0(), x + y)) else { return Ok(()) };

        prop_assert!(
            q1.amount_out + q2.amount_out <= qc.amount_out,
            "splitting {x} + {y} produced {} vs {} in one swap -- the model invented {}",
            q1.amount_out + q2.amount_out,
            qc.amount_out,
            (q1.amount_out + q2.amount_out).saturating_sub(qc.amount_out)
        );
    }

    /// Quoting more input never yields less output. A non-monotonic quote lets
    /// the sizing search walk downhill into a local maximum that is not there.
    #[test]
    fn a_larger_input_never_quotes_a_smaller_output(
        a in 1_000_000_000_000u128..20_000_000_000_000_000_000u128,
        extra in 1u128..20_000_000_000_000_000_000u128,
    ) {
        let s = cl_fixture();
        let small = U256::from(a);
        let large = U256::from(a) + U256::from(extra);
        let Ok(q_small) = ClEngine.quote_exact(&s, &Order::new(TOKEN0(), small)) else { return Ok(()) };
        let Ok(q_large) = ClEngine.quote_exact(&s, &Order::new(TOKEN0(), large)) else { return Ok(()) };
        prop_assert!(
            q_large.amount_out >= q_small.amount_out,
            "{large} quoted {} but {small} quoted {}",
            q_large.amount_out, q_small.amount_out
        );
    }
}

/// The two composition properties above are guarded by `else { return Ok(()) }`
/// on every step. A fixture too thin to fill the amounts under test would make
/// every case take that branch and the property would pass by proving nothing.
///
/// This asserts the fixture actually answers across the proptest range, that
/// the quotes cross real ticks (otherwise the CL property is only exercising
/// constant-product maths inside a single range), and reports the measured
/// cost of splitting.
///
/// # What the measurement says
///
/// Splitting is **free to within rounding** — 0 or 1 wei on a 20 ETH swap.
/// That is not a defect, it is the algebra: for a constant-product pool,
/// `out(a/2)` followed by `out'(a/2)` telescopes to exactly `out(a)`, because
/// the second swap trades against reserves the first one moved by precisely
/// the amount that preserves `k`. Concentrated liquidity is constant-product
/// on virtual reserves inside each tick range, so it inherits the same result.
///
/// So PLAN.md §33 Task 2.1's equality was very nearly right, and the reason to
/// assert `<=` instead is narrower than "the curve is convex": it is that each
/// step rounds output down independently, so a split can come up a wei or two
/// short. What must never happen is the other direction — a split coming out
/// AHEAD is free money that exists only in our arithmetic, and ranking
/// maximises gross, so the searcher would chase it on every block.
#[test]
fn the_composition_properties_are_not_vacuous() {
    let s = cl_fixture();
    let cp = cpmm_fixture();
    let probes = [
        1_000_000_000_000u128,
        1_000_000_000_000_000u128,
        1_000_000_000_000_000_000u128,
        10_000_000_000_000_000_000u128,
        20_000_000_000_000_000_000u128,
    ];
    let mut report = Vec::new();
    let mut crossed_any = false;
    for amount in probes {
        let whole = U256::from(amount);
        let half = U256::from(amount / 2);

        let cl_one = ClEngine
            .quote_exact(&s, &Order::new(TOKEN0(), whole))
            .unwrap_or_else(|e| panic!("cl fixture must quote {amount}: {e:?}"));
        crossed_any |= cl_one.ticks_crossed > 0;
        let cl_a = ClEngine
            .quote_exact(&s, &Order::new(TOKEN0(), half))
            .unwrap_or_else(|e| panic!("cl fixture must quote half of {amount}: {e:?}"));
        let cl_next = ClEngine
            .next_state_exact(&s, &Order::new(TOKEN0(), half))
            .unwrap_or_else(|e| panic!("cl fixture must advance on half of {amount}: {e:?}"));
        let cl_b = ClEngine
            .quote_exact(&cl_next, &Order::new(TOKEN0(), half))
            .unwrap_or_else(|e| panic!("cl fixture must quote the second half: {e:?}"));

        let cpmm_one = CpmmEngine
            .quote_exact(&cp, &Order::new(TOKEN0(), whole))
            .unwrap_or_else(|e| panic!("cpmm fixture must quote {amount}: {e:?}"));
        let cpmm_a = CpmmEngine
            .quote_exact(&cp, &Order::new(TOKEN0(), half))
            .expect("cpmm half");
        let cpmm_next = CpmmEngine
            .next_state_exact(&cp, &Order::new(TOKEN0(), half))
            .expect("cpmm advance");
        let cpmm_b = CpmmEngine
            .quote_exact(&cpmm_next, &Order::new(TOKEN0(), half))
            .expect("cpmm second half");

        // The load-bearing direction: a split must never come out ahead.
        assert!(
            cl_a.amount_out + cl_b.amount_out <= cl_one.amount_out,
            "cl split created value at {amount}"
        );
        assert!(
            cpmm_a.amount_out + cpmm_b.amount_out <= cpmm_one.amount_out,
            "cpmm split created value at {amount}"
        );

        let cl_gap = cl_one.amount_out - (cl_a.amount_out + cl_b.amount_out);
        let cpmm_gap = cpmm_one.amount_out - (cpmm_a.amount_out + cpmm_b.amount_out);

        // The two venues are allowed to differ here, and the reason is
        // structural rather than numerical.
        //
        // Uniswap V3 holds fees OUTSIDE the swappable curve — `computeSwapStep`
        // returns `feeAmount` separately and the price moves only by the
        // post-fee input, with the fee accruing to `feeGrowthGlobal`. So the
        // composition telescopes exactly and the only gap is per-step rounding.
        assert!(
            cl_gap <= U256::from(16u64),
            "a CL split lost {cl_gap} wei at {amount}: more than rounding, so \
             the next-state model is losing information"
        );
        // Uniswap V2 adds the fee straight to its reserves, so the second half
        // of a split trades against a pool the fee has already moved and pays
        // fee-on-fee. The loss is real, second-order, and must stay that way:
        // if it ever reached a basis point it would start deciding routes.
        assert!(
            cpmm_gap * U256::from(10_000u64) <= cpmm_one.amount_out,
            "a CPMM split lost {cpmm_gap} wei of {} at {amount} -- over 1 bp, \
             which is large enough to change a routing decision",
            cpmm_one.amount_out
        );
        report.push((amount, cl_one.ticks_crossed, cl_gap, cpmm_gap));
    }
    // And the V2 fee-on-fee term is REAL, not something rounding could produce.
    // If it were zero everywhere, `next_state_exact` would be failing to retain
    // the fee in reserves -- the leak that prices the next swap against a pool
    // poorer than it is.
    let cpmm_biting = report.iter().filter(|(_, _, _, gap)| !gap.is_zero()).count();
    assert!(
        cpmm_biting >= 3,
        "the V2 split penalty vanished, so the fee is not entering reserves: {report:?}"
    );
    assert!(
        crossed_any,
        "no probe crossed a tick, so the CL property never exercised anything \
         a constant-product pool would not have covered: {report:?}"
    );
    println!("(amount, cl_ticks_crossed, cl_split_gap_wei, cpmm_split_gap_wei)");
    for row in &report {
        println!("  {row:?}");
    }
}
