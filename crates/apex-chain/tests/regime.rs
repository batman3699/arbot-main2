//! Task 7.4 — §20.1, Blueprint §4.3, §4.5.
//!
//! > Adapters **discover** their active execution regime at startup and
//! > periodically thereafter rather than hard-coding a historical assumption.
//! > **A chain whose regime cannot be discovered is not admitted to live
//! > trading.**

use apex_chain::regime::{
    ChainRegime, FeeModel, NotDiscovered, OrderingMode, PriorityFeeSemantics, RegimeDiscovery,
    ReplacementRules,
};
use apex_types::ids::ChainId;
use apex_types::time::{DurationNanos, UnixNanos};

const T0: UnixNanos = UnixNanos(1_700_000_000_000_000_000);
const TTL: DurationNanos = DurationNanos(60_000_000_000);

fn base_regime(at: UnixNanos) -> RegimeDiscovery {
    RegimeDiscovery::discovered(
        ChainId::BASE,
        OrderingMode::Sequencer,
        PriorityFeeSemantics::RanksWithinWindow,
        DurationNanos(2_000_000_000),
        true,
        true,
        ReplacementRules::BumpRequired { min_bump_bps: 1_000 },
        FeeModel {
            has_l1_data_fee: true,
            l1_data_fee_uses_blobs: true,
            has_priority_fee: true,
            has_builder_payment: false,
        },
        at,
    )
}

/// **The plan's Task 7.4 Step 1 test.**
#[test]
fn a_chain_whose_regime_cannot_be_discovered_is_not_admitted() {
    for failure in [
        NotDiscovered::Unreachable { detail: "probe timed out".to_string() },
        NotDiscovered::Unrecognized { detail: "ordering mode not modelled".to_string() },
    ] {
        let d = RegimeDiscovery::Failed(failure.clone());
        assert_eq!(d.admit_to_live_trading(T0, TTL).err(), Some(failure));
    }
}

/// And a discovered one is.
#[test]
fn a_discovered_regime_is_admitted_and_says_what_it_found() {
    let d = base_regime(T0);
    let r = d.admit_to_live_trading(T0, TTL).expect("discovered");
    assert_eq!(r.chain, ChainId::BASE);
    assert_eq!(r.ordering_mode, OrderingMode::Sequencer);
    assert_eq!(r.priority_fee_semantics, PriorityFeeSemantics::RanksWithinWindow);
    assert!(r.fast_feed_available && r.private_feed_available);
    assert!(r.gas_and_data_fee_model.has_l1_data_fee);
    assert!(!r.gas_and_data_fee_model.has_builder_payment, "Base has no builder market");
}

/// **"and periodically thereafter" is the load-bearing half.** A regime
/// discovered once and never re-checked is a hard-coded assumption with extra
/// steps, so it ages out — and an aged one is refused rather than downgraded.
#[test]
fn a_regime_that_was_never_re_checked_stops_being_admissible() {
    let d = base_regime(T0);
    assert!(d.admit_to_live_trading(UnixNanos(T0.0 + TTL.0 - 1), TTL).is_ok());
    let err = d.admit_to_live_trading(UnixNanos(T0.0 + TTL.0 + 1), TTL).unwrap_err();
    let NotDiscovered::Stale { age, ttl } = err else { panic!("got {err:?}") };
    assert_eq!(ttl, TTL);
    assert!(age.0 > ttl.0);
}

/// Exactly at the TTL is still admissible; past it is not. An off-by-one here
/// either trades on a stale regime or refuses a fresh one.
#[test]
fn the_ttl_boundary_is_inclusive() {
    let d = base_regime(T0);
    assert!(d.admit_to_live_trading(UnixNanos(T0.0 + TTL.0), TTL).is_ok());
    assert!(d.admit_to_live_trading(UnixNanos(T0.0 + TTL.0 + 1), TTL).is_err());
}

/// **There is no `Assumed` variant, and no way to write a `ChainRegime` down.**
///
/// `ChainRegime`'s private `Discovered` marker means the only constructor is
/// `RegimeDiscovery::discovered`. A caller cannot assemble one from literals
/// and call it discovered — which is what makes §20.1's rule structural rather
/// than a paragraph somebody has to remember.
#[test]
fn a_regime_cannot_be_written_down() {
    // Compiles, because it goes through the constructor.
    let via_constructor = base_regime(T0);
    assert!(via_constructor.admit_to_live_trading(T0, TTL).is_ok());

    // The struct-literal form is a compile error; the compile_fail twin is in
    // `regime.rs`'s doctests. What is checkable here is that every field a
    // caller would need is readable but the marker is not constructible, so a
    // `..` update from an existing regime cannot be assembled either.
    let r: &ChainRegime = via_constructor.admit_to_live_trading(T0, TTL).expect("discovered");
    assert_eq!(r.discovered_at, T0, "a regime carries when it was observed");
}

/// A regime discovered later is fresher, which is the whole point of storing
/// the time.
#[test]
fn a_later_discovery_is_admissible_for_longer() {
    let early = base_regime(T0);
    let late = base_regime(UnixNanos(T0.0 + TTL.0));
    let now = UnixNanos(T0.0 + TTL.0 + 1);
    assert!(early.admit_to_live_trading(now, TTL).is_err());
    assert!(late.admit_to_live_trading(now, TTL).is_ok());
}
