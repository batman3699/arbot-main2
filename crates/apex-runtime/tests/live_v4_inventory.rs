//! Task 8.5 R37: v4 pools in the inventory. A record is a pool key, loaded
//! under its 20-byte key address; one the admission rules refuse, or whose key
//! collides with another pool's address, is dropped. The hooks allowed are a
//! reviewed list, `ops/v4-hooks.base.json`.

use alloy_primitives::{address, b256, Address, B256};
use apex_runtime::live::inventory::{load, UniverseFilter, Venue};
use apex_runtime::live::v4::{key_address, HookAllowlist, V4Key};
use std::collections::BTreeSet;
use std::path::Path;

const AXIS: &str = "0x5be75e3b0aff5d1a042a5363ae36acb1d6d755ea";
const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
const ALPHA: Address = address!("b0b24b89db0dafbe43c5b40226b63a179f592880");
const AXIS_POOL: B256 = b256!("c19d91d99afc0332e5013a569c9721cd9ecb51a14c851f7a189a140950a627fa");

fn record(id: &str, fee: u32, spacing: i32, hooks: &str) -> String {
    format!(
        r#"{{"pool_id":"{id}","currency0":"{AXIS}","currency1":"{USDC}","fee":{fee},"tick_spacing":{spacing},"hooks":"{hooks}","init_block":52800000,"hub_usd_liquidity":250000.0}}"#
    )
}

/// An inventory with every venue's file, empty but for `v3` and `v4`.
fn inventory(v3: &[String], v4: Option<&[String]>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for venue in Venue::ALL {
        if venue == Venue::UniswapV4 {
            continue;
        }
        std::fs::create_dir_all(dir.path().join(venue.dir())).unwrap();
        let body = if venue == Venue::UniswapV3 { v3.join("\n") } else { String::new() };
        std::fs::write(dir.path().join(venue.dir()).join("pools.jsonl"), body).unwrap();
    }
    if let Some(v4) = v4 {
        std::fs::create_dir_all(dir.path().join("uniswap_v4")).unwrap();
        std::fs::write(dir.path().join("uniswap_v4").join("pools.jsonl"), v4.join("\n")).unwrap();
    }
    dir
}

fn allow_alpha() -> HookAllowlist {
    HookAllowlist { hooks: [(ALPHA, B256::repeat_byte(7))].into_iter().collect() }
}

fn filter() -> UniverseFilter {
    UniverseFilter { max_fee_ppm: 3_000, min_depth_usd: 50_000.0 }
}

#[test]
fn v4_records_load_under_their_key_address() {
    let dir = inventory(&[], Some(&[record(&format!("{AXIS_POOL:#x}"), 100, 1, &format!("{ALPHA:#x}"))]));
    let specs = load(dir.path(), filter(), &BTreeSet::new(), &allow_alpha()).unwrap();
    assert_eq!(specs.len(), 1);
    let s = &specs[0];
    assert_eq!((s.venue, s.pool, s.fee_ppm), (Venue::UniswapV4, key_address(AXIS_POOL), 100));
    assert_eq!((s.token0, s.token1), (AXIS.parse().unwrap(), USDC.parse().unwrap()));
    assert_eq!(s.v4, Some(V4Key { pool_id: AXIS_POOL, fee: 100, tick_spacing: 1, hooks: ALPHA }));
}

#[test]
fn an_unlisted_hook_and_a_dynamic_fee_are_dropped() {
    let other = "0x1111111111111111111111111111111111111111111111111111111111111111";
    let dynamic = "0x2222222222222222222222222222222222222222222222222222222222222222";
    let dir = inventory(
        &[],
        Some(&[
            record(other, 100, 1, "0x00000000000000000000000000000000000000c0"),
            record(dynamic, 0x80_0000, 60, "0x0000000000000000000000000000000000000000"),
        ]),
    );
    assert!(load(dir.path(), filter(), &BTreeSet::new(), &allow_alpha()).unwrap().is_empty());
}

/// What `f` logs at `level` and above, as the binary's plain formatter writes it.
fn logged(level: tracing::Level, f: impl FnOnce()) -> String {
    #[derive(Clone)]
    struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buf = Buf(Default::default());
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt().with_ansi(false).with_max_level(level).with_writer(move || writer.clone()).finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = buf.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

fn three_refused() -> tempfile::TempDir {
    let unlisted = "0x00000000000000000000000000000000000000c0";
    inventory(
        &[],
        Some(&[
            record("0x1111111111111111111111111111111111111111111111111111111111111111", 100, 1, unlisted),
            record("0x3333333333333333333333333333333333333333333333333333333333333333", 100, 1, unlisted),
            record("0x2222222222222222222222222222222222222222222222222222222222222222", 0x80_0000, 60, "0x0000000000000000000000000000000000000000"),
        ]),
    )
}

/// The evaluator loads the inventory every five minutes. One WARN for each
/// refused pool came to about 22,000 lines a load and 6 GB of syslog a day
/// (2026-11-19). Now a load reports its refusals once, by reason.
#[test]
fn a_load_reports_its_refusals_in_one_line_by_reason() {
    let dir = three_refused();
    let logs = logged(tracing::Level::INFO, || assert!(load(dir.path(), filter(), &BTreeSet::new(), &allow_alpha()).unwrap().is_empty()));
    assert!(!logs.contains("WARN"), "a refusal is the admission rules working, not a fault:\n{logs}");
    let refused: Vec<&str> = logs.lines().filter(|l| l.contains("refused")).collect();
    assert_eq!(refused.len(), 1, "one line a load:\n{logs}");
    assert!(refused[0].contains("refused=3"), "{}", refused[0]);
    assert!(refused[0].contains("dynamic fee: 1, hook not allowlisted: 2"), "{}", refused[0]);
}

#[test]
fn each_refused_pool_is_named_at_debug() {
    let dir = three_refused();
    let logs = logged(tracing::Level::DEBUG, || drop(load(dir.path(), filter(), &BTreeSet::new(), &allow_alpha()).unwrap()));
    for id in ["0x1111", "0x2222", "0x3333"] {
        assert!(logs.lines().any(|l| l.contains("DEBUG") && l.contains(id)), "{id} at debug:\n{logs}");
    }
}

#[test]
fn a_load_that_refuses_nothing_says_nothing_of_refusals() {
    let dir = inventory(&[], Some(&[record(&format!("{AXIS_POOL:#x}"), 100, 1, &format!("{ALPHA:#x}"))]));
    let logs = logged(tracing::Level::INFO, || assert_eq!(load(dir.path(), filter(), &BTreeSet::new(), &allow_alpha()).unwrap().len(), 1));
    assert!(!logs.contains("refused"), "{logs}");
}

#[test]
fn a_key_colliding_with_an_inventoried_pool_is_dropped() {
    let v3 = format!(
        r#"{{"pool":"{:#x}","token0":"{AXIS}","token1":"{USDC}","fee":500,"hub_usd_liquidity":900000.0}}"#,
        key_address(AXIS_POOL)
    );
    let dir = inventory(&[v3], Some(&[record(&format!("{AXIS_POOL:#x}"), 100, 1, &format!("{ALPHA:#x}"))]));
    let specs = load(dir.path(), filter(), &BTreeSet::new(), &allow_alpha()).unwrap();
    assert_eq!(specs.len(), 1, "the v3 pool stays, the v4 key that collides with it goes");
    assert_eq!(specs[0].venue, Venue::UniswapV3);
}

#[test]
fn no_v4_inventory_is_no_v4_pools() {
    let dir = inventory(&[], None);
    assert!(load(dir.path(), filter(), &BTreeSet::new(), &allow_alpha()).unwrap().is_empty());
}

#[test]
fn the_shipped_hook_file_names_the_alpha_hook() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ops/v4-hooks.base.json");
    let list = HookAllowlist::load(&path).unwrap();
    assert_eq!(list.hooks.get(&ALPHA), Some(&b256!("db992c225ee2523445c10a64cf01daa3bd6800d97abd94752be7ab2328aff659")));
    assert_eq!(list.addresses(), [ALPHA].into_iter().collect());
}
