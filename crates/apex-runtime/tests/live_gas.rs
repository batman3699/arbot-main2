//! Task 8.5 R12 — what settling a route uses in gas, and the most it can.
//!
//! The model is checked against the 1,560 settlements of the deployed executor
//! it was measured from (`fixtures/settlement_gas.json`), so a change to its
//! figures that stops covering them fails here rather than as an out-of-gas
//! revert.

use apex_chain::base::adapter::gas_limit_over;
use apex_runtime::econ::GasEstimate;
use apex_runtime::live::gas::{HopSteps, PerVenue, SettlementGas, MEASURED, ZERO_SLOT_PREMIUM};
use apex_runtime::live::inventory::Venue;
use apex_types::cost::GasUsed;
use serde_json::Value;

fn hop(venue: Venue, zero_for_one: bool, crossed: u32, word_steps: u32) -> HopSteps {
    HopSteps { venue, zero_for_one, crossed, word_steps }
}

/// **A settlement is its hops and what they cross.** Each hop's own work by
/// venue, each crossing at its direction's mean, each word step; and for the
/// ceiling, each hop's margin, each crossing at its ceiling, each word step at
/// its own.
#[test]
fn a_settlement_is_its_hops_and_what_they_cross() {
    let m = SettlementGas {
        hop: PerVenue { uniswap_v3: 100_000, slipstream: 200_000, pancake_v3: 300_000, aerodrome_v2: 400_000 },
        crossing_down: PerVenue { uniswap_v3: 1_000, slipstream: 2_000, pancake_v3: 3_000, aerodrome_v2: 4_000 },
        crossing_up: PerVenue { uniswap_v3: 10_000, slipstream: 20_000, pancake_v3: 30_000, aerodrome_v2: 40_000 },
        word: 7,
        hop_margin: 500,
        crossing_ceiling: PerVenue { uniswap_v3: 40_000, slipstream: 50_000, pancake_v3: 60_000, aerodrome_v2: 70_000 },
        word_ceiling: 900,
    };
    let got = m.estimate(&[hop(Venue::Slipstream, true, 3, 1), hop(Venue::PancakeV3, false, 2, 2)]);
    assert_eq!(
        got,
        GasEstimate {
            expected: GasUsed(200_000 + 3 * 2_000 + 7 + 300_000 + 2 * 30_000 + 2 * 7),
            ceiling: GasUsed(200_000 + 500 + 3 * 50_000 + 900 + 300_000 + 500 + 2 * 60_000 + 2 * 900),
        }
    );
    // Uniswap's figures, and a hop's direction choosing its mean.
    let uni = m.estimate(&[hop(Venue::UniswapV3, false, 1, 0), hop(Venue::UniswapV3, true, 1, 0)]);
    assert_eq!(uni.expected, GasUsed(100_000 + 10_000 + 100_000 + 1_000));
    assert_eq!(uni.ceiling, GasUsed(2 * (100_000 + 500 + 40_000)));
}

/// The ceiling is never below the expectation, whatever a model says.
#[test]
fn the_ceiling_is_never_below_the_expectation() {
    let m = SettlementGas { crossing_ceiling: PerVenue { uniswap_v3: 0, slipstream: 0, pancake_v3: 0, aerodrome_v2: 0 }, ..MEASURED };
    let got = m.estimate(&[hop(Venue::UniswapV3, false, 5, 0), hop(Venue::PancakeV3, true, 5, 0)]);
    assert_eq!(got.ceiling, got.expected);
}

/// **A crossing's ceiling is a seasoned crossing with every slot it can write
/// from zero.** Uniswap writes two accumulators that can be zero — the fees'
/// outside each token — and PancakeSwap and Slipstream a third, their farm's
/// and their gauge's reward. The falling-price mean is the seasoned crossing:
/// every tick below a price has been crossed or written before.
#[test]
fn a_crossings_ceiling_writes_every_slot_it_can_from_zero() {
    assert_eq!(ZERO_SLOT_PREMIUM, 22_100 - 5_000);
    let c = MEASURED.crossing_ceiling;
    let d = MEASURED.crossing_down;
    assert_eq!(c.uniswap_v3, d.uniswap_v3 + 2 * ZERO_SLOT_PREMIUM);
    assert_eq!(c.pancake_v3, d.pancake_v3 + 3 * ZERO_SLOT_PREMIUM);
    assert_eq!(c.slipstream, d.slipstream + 3 * ZERO_SLOT_PREMIUM);
    for v in Venue::ALL {
        assert!(MEASURED.crossing_ceiling.of(v) >= MEASURED.crossing_up.of(v), "{v:?}");
    }
}

fn venue(name: &str) -> Venue {
    match name {
        "uniswap_v3" => Venue::UniswapV3,
        "slipstream" => Venue::Slipstream,
        "pancake_v3" => Venue::PancakeV3,
        "aerodrome_v2" => Venue::AerodromeV2,
        other => panic!("unknown venue {other}"),
    }
}

/// The recorded settlements: each one's two hops and the gas it used.
fn recorded() -> Vec<([HopSteps; 2], u64)> {
    let f: Value = serde_json::from_str(include_str!("fixtures/settlement_gas.json")).unwrap();
    f["trades"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            let h = |i: usize| {
                hop(
                    venue(t[i].as_str().unwrap()),
                    t[i + 1].as_bool().unwrap(),
                    u32::try_from(t[i + 2].as_u64().unwrap()).unwrap(),
                    u32::try_from(t[i + 3].as_u64().unwrap()).unwrap(),
                )
            };
            ([h(0), h(4)], t[8].as_u64().unwrap())
        })
        .collect()
}

/// **No recorded settlement used more than its ceiling.** 1,560 settlements of
/// the deployed executor, all three venues, 0 to 36 crossings a hop, the worst
/// climbing pools nothing had touched in the block: each one's ceiling covers
/// it, so the limit built on it — the ceiling and §21.3's headroom — would have
/// run none of them out of gas.
#[test]
fn no_recorded_settlement_used_more_than_its_ceiling() {
    let trades = recorded();
    assert_eq!(trades.len(), 1_560);
    let mut worst = 0.0f64;
    for (hops, gas) in &trades {
        let e = MEASURED.estimate(hops);
        assert!(*gas <= e.ceiling.0, "{hops:?} used {gas}, over its ceiling {}", e.ceiling.0);
        assert!(*gas <= gas_limit_over(e.ceiling).0);
        #[allow(clippy::cast_precision_loss)]
        let used = *gas as f64 / e.ceiling.0 as f64;
        worst = worst.max(used);
    }
    // Not so loose as to say nothing: the worst comes within 10% of it.
    assert!(worst > 0.9, "the worst used {worst:.3} of its ceiling");
    // The fixture is the case that motivated this: a fixed limit, the one every
    // ticket had before, runs a large share of real trades out of gas.
    assert!(trades.iter().filter(|(_, gas)| *gas > 777_650).count() > 400);
}

/// **The expected figure is the recorded mean.** What the EV prices must not
/// drift from what settlements use: over the recorded ones the expectation
/// misses by under half a percent on average, and by under a third on any one.
#[test]
fn the_expected_figure_is_the_recorded_mean() {
    let trades = recorded();
    let (mut sum_actual, mut sum_expected) = (0u128, 0u128);
    for (hops, gas) in &trades {
        let e = MEASURED.estimate(hops).expected.0;
        sum_actual += u128::from(*gas);
        sum_expected += u128::from(e);
        #[allow(clippy::cast_precision_loss)]
        let ratio = *gas as f64 / e as f64;
        assert!((0.8..1.35).contains(&ratio), "{hops:?}: used {gas}, expected {e}");
    }
    let drift = sum_actual.abs_diff(sum_expected) * 1_000 / sum_actual;
    assert!(drift < 5, "the expectation is {drift} per mille off the recorded total");
}
