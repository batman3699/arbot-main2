//! R22: Aerodrome's second Slipstream factory is a venue of its own; R24:
//! Aerodrome v2's volatile pools are another.

use alloy_primitives::address;
use apex_runtime::live::calls::binding;
use apex_runtime::live::feed::{swap_topic, SYNC};
use apex_runtime::live::frontier::{AERODROME_V2_ADAPTER, AERODROME_V2_ROUTER};
use apex_runtime::live::gas::{self, HopSteps};
use apex_runtime::live::inventory::{self, UniverseFilter, Venue};
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

// ------------------------------------------------------------------ R24

#[test]
fn aerodromes_volatile_pools_are_a_venue_of_their_own() {
    let v = Venue::AerodromeV2;
    assert!(Venue::ALL.contains(&v));
    assert_eq!(v.dir(), "aerodrome_v2");
    assert_eq!(v.factory(), address!("420DD381b31aEf6683db6B902084cB0FFECe40Da"));
    assert_eq!(v.id(), venue_ids::AERODROME_VOLATILE);
    assert!(v.fee_is_static(), "no TWAP: the factory sets it, re-read each head");
    assert!(v.is_constant_product());
    assert!(Venue::ALL.iter().filter(|v| v.is_constant_product()).eq([Venue::AerodromeV2].iter()));
}

/// A volatile pool's state arrives in `Sync`, not `Swap`.
#[test]
fn its_state_arrives_in_sync() {
    assert_eq!(swap_topic(Venue::AerodromeV2), SYNC);
    assert_eq!(SYNC, alloy_primitives::keccak256(b"Sync(uint256,uint256)"));
}

/// No ticks: a hop is its fixed figure, crossing nothing.
#[test]
fn it_settles_at_one_figure_a_hop() {
    let aero = HopSteps { venue: Venue::AerodromeV2, zero_for_one: true, crossed: 0, word_steps: 0 };
    let e = gas::MEASURED.estimate(&[aero]);
    assert_eq!(e.expected.0, gas::MEASURED.hop.aerodrome_v2);
    assert_eq!(e.ceiling.0, gas::MEASURED.hop.aerodrome_v2 + gas::MEASURED.hop_margin);
}

#[test]
fn it_binds_adapter_four_at_aerodromes_router() {
    let b = binding(Venue::AerodromeV2).expect("an adapter venue");
    assert_eq!((b.id, b.router), (AERODROME_V2_ADAPTER, AERODROME_V2_ROUTER));
    assert_eq!(b.id, 4);
    assert_eq!(AERODROME_V2_ROUTER, address!("cF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43"));
    assert_eq!(b.selector, [0xca, 0xc8, 0x8e, 0xa9]);
}

/// Only volatile records load: a stable one, or one that does not say, is refused.
#[test]
fn the_inventory_takes_volatile_aerodrome_records_only() {
    let dir = tempfile::tempdir().unwrap();
    for v in Venue::ALL {
        std::fs::create_dir_all(dir.path().join(v.dir())).unwrap();
        std::fs::write(dir.path().join(v.dir()).join("pools.jsonl"), "").unwrap();
    }
    let rec = |n: u8, stable: Option<bool>| {
        let mut r = serde_json::json!({ "pool": format!("0x{n:040x}"), "token0": "0x4200000000000000000000000000000000000006",
            "token1": "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913", "fee": 30, "fee_ppm_onchain": 3000,
            "hub_usd_liquidity": 1e6, "hub_symbol": "WETH" });
        if let Some(s) = stable {
            r["stable"] = serde_json::json!(s);
        }
        r.to_string()
    };
    let lines = [rec(1, Some(false)), rec(2, Some(true)), rec(3, None)];
    std::fs::write(dir.path().join("aerodrome_v2").join("pools.jsonl"), lines.join("\n")).unwrap();
    let filter = UniverseFilter { max_fee_ppm: 3_000, min_depth_usd: 100_000.0 };
    let got: Vec<u8> = inventory::load(dir.path(), filter).unwrap().iter().map(|s| s.pool.as_slice()[19]).collect();
    assert_eq!(got, vec![1]);
}
