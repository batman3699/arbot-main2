//! Task 2.3 — every admitted venue has an adapter, and the breakers are
//! independent of one another.

use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_math::engine::{ClEdgeState, CpmmState, Order, PricingError, SolidlyState};
use apex_math::quote_solidly::SolidlyPairState;
use apex_types::ids::{ChainId, PoolId, VenueId};
use apex_types::route::Exactness;
use apex_types::sim::RevertClass;
use apex_types::time::UnixNanos;
use apex_venues::adapter::{
    all_adapters, venue_ids, AerodromeAdapter, BalancerAdapter, CurveAdapter, GasProvenance,
    PancakeV3Adapter, SlipstreamAdapter, UniV2Adapter, UniV3Adapter, VenueAdapter, VenueState,
};
use apex_venues::breaker::{BreakerPolicy, BreakerRegistry, BreakerState};
use ethers_core::types::{Address, U256};

fn token(n: u8) -> Address {
    let mut b = [0u8; 20];
    b[19] = n;
    Address::from(b)
}

fn pool_id() -> PoolId {
    PoolId {
        chain: ChainId::BASE,
        address: alloy_primitives::Address::repeat_byte(0x11),
    }
}

fn cl_state() -> VenueState {
    VenueState::ConcentratedLiquidity(Box::new(ClEdgeState {
        pool: token(10),
        token0: token(1),
        token1: token(2),
        state: ClPoolState {
            sqrt_price_x96: U256::from(1u128) << 96,
            liquidity: 1_000_000_000_000_000_000_000,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
            balance0: None,
            balance1: None,
        },
        ladder: TickLadder::new(
            vec![
                (-600, 300_000_000_000_000_000_000),
                (-60, 100_000_000_000_000_000_000),
                (60, -100_000_000_000_000_000_000),
                (600, -300_000_000_000_000_000_000),
            ],
            -600,
            600,
        ),
        max_ticks: 128,
    }))
}

fn cpmm_state() -> VenueState {
    VenueState::ConstantProduct(CpmmState {
        pool: token(11),
        token0: token(1),
        token1: token(2),
        reserve0: U256::from(1_000_000_000_000_000_000_000u128),
        reserve1: U256::from(2_000_000_000_000_000_000_000u128),
        fee_bps: 30,
    })
}

fn solidly_state() -> VenueState {
    VenueState::Solidly(Box::new(SolidlyState {
        pool: token(12),
        pair: SolidlyPairState {
            token0: token(1),
            token1: token(2),
            reserve0: U256::from(1_000_000_000_000_000_000_000u128),
            reserve1: U256::from(2_000_000_000_000_000_000_000u128),
            stable: false,
            decimals0: 18,
            decimals1: 18,
        },
        fee_bps: 5,
    }))
}

fn order() -> Order {
    Order::new(token(1), U256::from(1_000_000_000_000_000_000u128))
}

/// The Base universe §10.5 names, minus Uniswap V4 (Phase 11).
#[test]
fn every_admitted_venue_has_an_adapter() {
    let ids: Vec<VenueId> = all_adapters().iter().map(|a| a.venue_id()).collect();
    for required in [
        venue_ids::UNISWAP_V3,
        venue_ids::AERODROME_SLIPSTREAM,
        venue_ids::PANCAKESWAP_V3,
        venue_ids::UNISWAP_V2,
        venue_ids::AERODROME_VOLATILE,
        venue_ids::CURVE,
        venue_ids::BALANCER,
    ] {
        assert!(ids.contains(&required), "no adapter for {required:?}");
    }
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len(), "two adapters share a venue id: {ids:?}");
}

/// Every adapter answers every method. `gas_model` and `classify_revert` have
/// no default bodies, so this compiling is half the assertion; the other half
/// is that the answers are not placeholders.
#[test]
fn every_adapter_answers_the_whole_contract() {
    for a in all_adapters() {
        let needs = a.identify_state_dependencies(pool_id());
        assert_eq!(needs.pool, pool_id());
        assert!(needs.reads_spot, "{:?} must read price or reserves", a.venue_id());

        let gas = a.gas_model();
        assert!(gas.per_hop.0 >= 100_000, "{:?} gas looks unset", a.venue_id());

        assert_eq!(
            a.classify_revert(&[]),
            RevertClass::Unknown,
            "{:?} must not guess at empty revert data",
            a.venue_id()
        );
    }
}

/// Every gas figure is an inherited constant, and says so.
///
/// This test is expected to CHANGE in Phase 3, which measures them. Until it
/// does, the assertion documents that not one of the six numbers the profit
/// decision rests on has a recorded measurement behind it.
#[test]
fn no_venue_gas_figure_has_been_measured_yet() {
    for a in all_adapters() {
        assert_eq!(
            a.gas_model().provenance,
            GasProvenance::UnmeasuredLegacyConstant,
            "{:?} claims measured gas -- update this test with the measurement",
            a.venue_id()
        );
    }
}

#[test]
fn the_priceable_venues_quote_and_claim_proven_exactness() {
    let cases: Vec<(Box<dyn VenueAdapter>, VenueState)> = vec![
        (Box::new(UniV3Adapter), cl_state()),
        (Box::new(SlipstreamAdapter), cl_state()),
        (Box::new(PancakeV3Adapter), cl_state()),
        (Box::new(UniV2Adapter), cpmm_state()),
        (Box::new(AerodromeAdapter), solidly_state()),
    ];
    for (adapter, state) in cases {
        let q = adapter
            .quote_exact(&state, &order())
            .unwrap_or_else(|e| panic!("{:?} failed to quote: {e:?}", adapter.venue_id()));
        assert!(q.quote.amount_out > U256::zero());
        assert_eq!(q.exactness, Exactness::Proven);
        assert!(q.exactness.may_authorize_live_dispatch());
    }
}

/// Curve and Balancer refuse rather than forwarding a router quote.
///
/// Both are `abigen!` clients with no local curve implementation. A quote that
/// passed the on-chain quoter's number through and called itself exact would
/// make INV-16 -- "no router quote is authoritative" -- a dead letter.
#[test]
fn venues_with_no_local_maths_refuse_instead_of_forwarding_a_router_quote() {
    for adapter in [
        Box::new(CurveAdapter) as Box<dyn VenueAdapter>,
        Box::new(BalancerAdapter),
    ] {
        let err = adapter
            .quote_exact(&cpmm_state(), &order())
            .expect_err("must not answer");
        match err {
            PricingError::NotRepresentable(why) => {
                assert!(why.contains("no local implementation"), "{why}")
            }
            other => panic!("expected NotRepresentable, got {other:?}"),
        }
    }
}

/// An adapter handed the wrong state kind refuses; it does not reinterpret it.
#[test]
fn an_adapter_given_the_wrong_state_kind_refuses() {
    let err = UniV3Adapter
        .quote_exact(&cpmm_state(), &order())
        .expect_err("a CL adapter cannot price a constant-product pool");
    assert!(matches!(err, PricingError::NotRepresentable(_)));

    let err = UniV2Adapter
        .quote_exact(&cl_state(), &order())
        .expect_err("a CPMM adapter cannot price a CL pool");
    assert!(matches!(err, PricingError::NotRepresentable(_)));
}

/// §10.5: "Every venue gets an independent circuit breaker."
#[test]
fn each_adapter_has_a_breaker_that_trips_without_affecting_the_others() {
    let now = UnixNanos(0);
    let reg = BreakerRegistry::new(BreakerPolicy::default());
    let adapters = all_adapters();

    // Trip exactly one venue, structurally.
    let victim = adapters[0].venue_id();
    let breaker = reg.for_venue(victim);
    for _ in 0..BreakerPolicy::default().trip_after {
        breaker
            .try_admit(now)
            .expect("closed")
            .revert(RevertClass::Unauthorized, now);
    }
    assert_eq!(breaker.state(now), BreakerState::Open);

    for a in adapters.iter().skip(1) {
        let other = reg.for_venue(a.venue_id());
        assert_eq!(
            other.state(now),
            BreakerState::Closed,
            "{:?} tripped because {victim:?} did",
            a.venue_id()
        );
        assert!(other.try_admit(now).is_some());
    }
    assert_eq!(reg.open_venues(now), vec![victim]);
}

/// The breaker's input comes from the adapter's own classification, so the two
/// have to agree about what a min-out failure is.
#[test]
fn a_min_out_revert_from_an_adapter_does_not_trip_its_breaker() {
    let now = UnixNanos(0);
    let reg = BreakerRegistry::new(BreakerPolicy::default());
    let adapter = UniV3Adapter;
    let breaker = reg.for_venue(adapter.venue_id());

    // The exact bytes a Uniswap V3 SwapRouter emits when it loses the race.
    let mut data = vec![0x08, 0xc3, 0x79, 0xa0];
    data.extend(ethers_core::abi::encode(&[ethers_core::abi::Token::String(
        "Too little received".to_string(),
    )]));
    let class = adapter.classify_revert(&data);
    assert_eq!(class, RevertClass::MinOutNotMet);

    for _ in 0..50 {
        breaker.try_admit(now).expect("closed").revert(class, now);
    }
    assert_eq!(
        breaker.state(now),
        BreakerState::Closed,
        "losing 50 races must not disable the venue we are winning on"
    );
}
