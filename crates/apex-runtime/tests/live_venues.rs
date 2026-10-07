//! R22: Aerodrome's second Slipstream factory is a venue of its own.

use alloy_primitives::address;
use apex_runtime::live::feed::swap_topic;
use apex_runtime::live::gas::{self, HopSteps};
use apex_runtime::live::inventory::Venue;
use apex_venues::adapter::venue_ids;

#[test]
fn the_second_slipstream_factory_is_its_own_venue() {
    let v = Venue::SlipstreamV3;
    assert!(Venue::ALL.contains(&v));
    assert_eq!(v.dir(), "aerodrome_slipstream_v3");
    assert_eq!(v.factory(), address!("f8f2eB4940CFE7d13603DDDD87f123820Fc061Ef"));
    assert_eq!(v.id(), venue_ids::AERODROME_SLIPSTREAM_V3);
    assert!(!v.fee_is_static(), "its fee module is dynamic, as the first factory's is");
}

#[test]
fn its_pools_emit_the_first_factorys_swap_topic() {
    assert_eq!(swap_topic(Venue::SlipstreamV3), swap_topic(Venue::Slipstream));
}

/// The same router and pool code as the first factory, so the same gas.
#[test]
fn it_settles_at_the_first_factorys_gas() {
    let hop = |venue| HopSteps { venue, zero_for_one: true, crossed: 3, word_steps: 1 };
    let uni = HopSteps { venue: Venue::UniswapV3, zero_for_one: false, crossed: 1, word_steps: 0 };
    assert_eq!(
        gas::MEASURED.estimate(&[hop(Venue::SlipstreamV3), uni]),
        gas::MEASURED.estimate(&[hop(Venue::Slipstream), uni]),
    );
}
