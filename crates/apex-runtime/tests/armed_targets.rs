//! Task 8.5 R36 — the census's targets in the runtime: the routes where
//! trigger-block arbitrage our executor can run happens, their pools admitted
//! below the depth floor, and only their cycles armed.

use alloy_primitives::{address, Address, B256};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::armed::targets::Targets;
use apex_runtime::live::book::PoolSnapshot;
use apex_runtime::live::frontier::{self, WETH};
use apex_runtime::live::inventory::{self, PoolSpec, UniverseFilter, Venue};
use apex_runtime::shadow::config::{ArmedConfig, ArmedMode};
use apex_types::ids::ChainId;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const SLIP: Address = address!("b2cc224c1c9feE385f8ad6a55b4d94E92359DC59");
/// 2026-10-24T01:10:00Z.
const GENERATED: u64 = 1_792_804_200;
const DAY2: u64 = 172_800;

fn p(n: u8) -> Address {
    Address::with_last_byte(n)
}

fn targets_json(generated_at: &str, ok: bool, routes: &str, bids: &str) -> String {
    format!(r#"{{"generated_at":"{generated_at}","days":["2026-10-21","2026-10-22","2026-10-23"],"top":50,"ok":{ok},
               "routes":[{routes}],"bids_wei_per_gas":[{bids}],"calibration":[]}}"#)
}

fn route(a: Address, b: Address, bids: Option<&str>) -> String {
    let own = bids.map(|b| format!(r#","bids_wei_per_gas":[{b}]"#)).unwrap_or_default();
    format!(r#"{{"pools":["{a:#x}","{b:#x}"],"usd":12.5,"arbs":3{own}}}"#)
}

fn write(dir: &Path, text: &str) -> std::path::PathBuf {
    let path = dir.join("targets.json");
    std::fs::write(&path, text).unwrap();
    path
}

fn load(text: &str, now: u64) -> Result<Targets, String> {
    let dir = tempfile::tempdir().unwrap();
    Targets::load(&write(dir.path(), text), now, DAY2)
}

/// One venue's inventory: `(pool, depth)` WETH/USDC records at 500 ppm.
fn inventory_dir(records: &[(u8, f64)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let lines: Vec<String> = records
        .iter()
        .map(|(n, depth)| {
            serde_json::json!({ "pool": format!("{:#x}", p(*n)), "token0": format!("{WETH:#x}"), "token1": format!("{USDC:#x}"),
                                "fee": 500, "fee_ppm_onchain": 500, "hub_usd_liquidity": depth })
            .to_string()
        })
        .collect();
    for v in ["uniswap_v3", "aerodrome_slipstream", "pancakeswap_v3", "aerodrome_slipstream_v3", "aerodrome_v2"] {
        std::fs::create_dir_all(dir.path().join(v)).unwrap();
        let body = if v == "uniswap_v3" { lines.join("\n") } else { String::new() };
        std::fs::write(dir.path().join(v).join("pools.jsonl"), body).unwrap();
    }
    dir
}

fn loaded(dir: &Path, include: &BTreeSet<Address>) -> Vec<u8> {
    let mut got: Vec<u8> =
        inventory::load(dir, UniverseFilter::default(), include, &apex_runtime::live::v4::HookAllowlist::default()).unwrap().iter().map(|s| s.pool.as_slice()[19]).collect();
    got.sort();
    got
}

#[test]
fn a_target_pool_below_the_floor_is_admitted_and_others_are_not() {
    // The floor is $100k: pools 1 and 2 hold $5k, pool 3 a million.
    let dir = inventory_dir(&[(1, 5e3), (2, 5e3), (3, 1e6)]);
    assert_eq!(loaded(dir.path(), &BTreeSet::new()), vec![3]);
    assert_eq!(loaded(dir.path(), &BTreeSet::from([p(1)])), vec![1, 3]);
}

#[test]
fn a_target_pool_without_an_inventory_record_is_counted_not_admitted() {
    let dir = inventory_dir(&[(1, 5e3), (3, 1e6)]);
    let t = load(&targets_json("2026-10-24T01:10:00Z", true, &route(p(1), p(9), None), "1"), GENERATED).unwrap();
    let specs = inventory::load(dir.path(), UniverseFilter::default(), &t.pools(), &apex_runtime::live::v4::HookAllowlist::default()).unwrap();
    assert_eq!(specs.iter().map(|s| s.pool.as_slice()[19]).collect::<BTreeSet<_>>(), BTreeSet::from([1, 3]));
    assert_eq!(apex_runtime::shadow::targets_missing(&t, &specs), 1, "pool 9 has no record");
}

#[test]
fn routes_are_matched_weth_in_pool_first() {
    let t = load(&targets_json("2026-10-24T01:10:00Z", true, &route(p(1), p(2), None), "1"), GENERATED).unwrap();
    assert!(t.has(p(1), p(2)));
    assert!(!t.has(p(2), p(1)), "the other direction is another route");
    assert_eq!(t.pools(), BTreeSet::from([p(1), p(2)]));
}

#[test]
fn a_route_without_twenty_bids_bids_against_everyone_s() {
    let routes = [route(p(1), p(2), Some("5,6")), route(p(3), p(4), None)].join(",");
    let t = load(&targets_json("2026-10-24T01:10:00Z", true, &routes, "1,2,3"), GENERATED).unwrap();
    assert_eq!(t.bids_for(p(1), p(2)), &[5, 6]);
    assert_eq!(t.bids_for(p(3), p(4)), &[1, 2, 3]);
    assert_eq!(t.bids_for(p(7), p(8)), &[1, 2, 3], "a route not targeted bids against everyone's too");
}

#[test]
fn a_stale_or_malformed_file_is_refused() {
    let good = targets_json("2026-10-24T01:10:00Z", true, &route(p(1), p(2), None), "1");
    assert!(load(&good, GENERATED + DAY2).is_ok(), "exactly the allowed age");
    let err = load(&good, GENERATED + DAY2 + 1).unwrap_err();
    assert!(err.contains("stale"), "{err}");
    assert!(load(&targets_json("2026-10-24T01:10:00Z", true, &route(p(1), p(1), None), "1"), GENERATED).is_err(), "equal pools");
    assert!(load(&good.replace("12.5", "NaN"), GENERATED).is_err(), "a NaN in the text");
    assert!(load(&good.replace("12.5", "1e400"), GENERATED).is_err(), "a value past f64");
    assert!(load(&good.replace("12.5", "-1.0"), GENERATED).is_err(), "a negative value");
    assert!(load(&good.replace("2026-10-24T01:10:00Z", "yesterday"), GENERATED).is_err(), "an unreadable time");
}

/// The arming engine's settings in `mode`. Built directly: the config file
/// takes `mode: live` only under `apex canary`, with its section.
fn armed(mode: ArmedMode, targets: Option<&Path>) -> ArmedConfig {
    ArmedConfig {
        mode,
        ledger: "var/a.jsonl".into(),
        verdicts: "var/v.jsonl".into(),
        max_arms: 16,
        rungs: 6,
        window_s: 10,
        block_time_ms: 2000,
        margin_wei: 0,
        band_cap_bps: 5,
        liquidity_floor_bps: 9800,
        targets: targets.map(Path::to_path_buf),
        targets_max_age_s: DAY2,
        max_priority_wei_per_gas: 20_000_000_000,
        daily_loss_budget_wei: 10_000_000_000_000_000,
        max_size_wei: 5_000_000_000_000_000_000,
        max_size_usdc: 12_000_000_000,
        l1_fee_bound_wei: 50_000_000_000_000,
        sender_reserve_wei: 200_000_000_000_000,
        live: None,
        moves: None,
        executor2: None,
        eval_simulations_per_run: 2,
    }
}

fn snapshot(addr: Address, venue: Venue) -> PoolSnapshot {
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: USDC, fee_ppm: 500, depth_usd: 5e6, v4: None },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(-197_350).unwrap(),
            liquidity: 1_400_000_000_000_000_000,
            tick: -197_350,
            tick_spacing: 10,
            fee_ppm: 500,
            balance0: None,
            balance1: None,
        },
        ladder: TickLadder::new(vec![(-199_000, 1), (-196_000, -1)], -200_000, -195_000),
        decimals: (18, 6),
        factory: venue.factory(),
        code_hash: B256::repeat_byte(0xc0),
        block: 100,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
        reserves: None,
    }
}

#[test]
fn targets_absent_or_unfit_is_fatal_live_and_falls_back_in_shadow() {
    use apex_runtime::shadow::{armed_cycles, load_targets};
    let dir = tempfile::tempdir().unwrap();
    let fit = write(dir.path(), &targets_json("2026-10-24T01:10:00Z", true, &route(UNI, SLIP, None), "1"));
    let unfit_dir = tempfile::tempdir().unwrap();
    let unfit = write(unfit_dir.path(), &targets_json("2026-10-24T01:10:00Z", false, &route(UNI, SLIP, None), "1"));
    let empty_dir = tempfile::tempdir().unwrap();
    let empty = write(empty_dir.path(), &targets_json("2026-10-24T01:10:00Z", true, "", "1"));

    assert!(load_targets(&armed(ArmedMode::Live, None), GENERATED).is_err(), "live needs targets");
    assert!(load_targets(&armed(ArmedMode::Live, Some(&unfit)), GENERATED).is_err(), "live refuses ok: false");
    assert!(load_targets(&armed(ArmedMode::Live, Some(&empty)), GENERATED).is_err(), "live refuses no routes");
    assert!(load_targets(&armed(ArmedMode::Live, Some(&fit)), GENERATED + DAY2 + 1).is_err(), "live refuses a stale file");
    assert!(load_targets(&armed(ArmedMode::Live, Some(&fit)), GENERATED).unwrap().is_some());
    assert!(load_targets(&armed(ArmedMode::Shadow, None), GENERATED).unwrap().is_none());
    assert!(load_targets(&armed(ArmedMode::Shadow, Some(&unfit)), GENERATED).unwrap().is_none(), "shadow falls back");
    assert!(load_targets(&armed(ArmedMode::Shadow, Some(&fit)), GENERATED + DAY2 + 1).unwrap().is_none(), "shadow falls back");

    let book: BTreeMap<Address, Arc<PoolSnapshot>> =
        [snapshot(UNI, Venue::UniswapV3), snapshot(SLIP, Venue::Slipstream)].into_iter().map(|s| (s.spec.pool, Arc::new(s))).collect();
    let (_, all) = frontier::build(ChainId::BASE, WETH, &book);
    assert_eq!(all.len(), 2, "UNI then SLIP, and SLIP then UNI");
    assert_eq!(armed_cycles(None, &all).len(), 2, "without targets, R26's cycle set");
    let t = load_targets(&armed(ArmedMode::Live, Some(&fit)), GENERATED).unwrap().unwrap();
    let armed = armed_cycles(Some(&t), &all);
    assert_eq!(armed.len(), 1);
    let c = armed.values().next().unwrap();
    assert_eq!((c.legs[0].pool, c.legs[1].pool), (UNI, SLIP), "WETH goes into UNI first");
}

/// R37: a v4 pool is named by its 32-byte ID and parsed to the address the
/// book keys it by; a route says its start token, WETH when it does not.
#[test]
fn a_v4_route_parses_to_its_key_address_and_start() {
    use apex_runtime::live::v4::key_address;
    let id = B256::repeat_byte(0xc1);
    let v4 = format!(r#"{{"pools":["{id:#x}","{:#x}"],"usd":30.0,"arbs":3,"start":"{USDC:#x}"}}"#, p(2));
    let routes = [v4, route(p(1), p(2), None)].join(",");
    let t = load(&targets_json("2026-10-24T01:10:00Z", true, &routes, "1"), GENERATED).unwrap();
    assert_eq!(t.routes[0].pools, [key_address(id), p(2)]);
    assert_eq!(t.routes[0].v4_ids, vec![id]);
    assert_eq!(t.routes[0].start, USDC);
    assert!(t.has(key_address(id), p(2)));
    assert_eq!((t.routes[1].start, t.routes[1].v4_ids.is_empty()), (WETH, true), "no start: R36's WETH");
    assert_eq!(t.pools(), BTreeSet::from([key_address(id), p(1), p(2)]));
    let bad = format!(r#"{{"pools":["0x{}","{:#x}"],"usd":1.0,"arbs":1}}"#, "c1".repeat(31), p(2));
    assert!(load(&targets_json("2026-10-24T01:10:00Z", true, &bad, "1"), GENERATED).is_err(), "neither an address nor an ID");
}

/// R37: a route arms the cycles in its own start token: WETH into a
/// WETH/USDC pool and USDC into it are opposite trades.
#[test]
fn a_route_arms_only_its_start_token_s_cycles() {
    use apex_runtime::shadow::armed_cycles;
    let book: BTreeMap<Address, Arc<PoolSnapshot>> =
        [snapshot(UNI, Venue::UniswapV3), snapshot(SLIP, Venue::Slipstream)].into_iter().map(|s| (s.spec.pool, Arc::new(s))).collect();
    let (_, all) = frontier::build_starts(ChainId::BASE, &[WETH, USDC], &book);
    assert_eq!(all.len(), 4, "two directions, two start tokens");
    let usdc = format!(r#"{{"pools":["{UNI:#x}","{SLIP:#x}"],"usd":3.0,"arbs":1,"start":"{USDC:#x}"}}"#);
    let t = load(&targets_json("2026-10-24T01:10:00Z", true, &usdc, "1"), GENERATED).unwrap();
    let armed = armed_cycles(Some(&t), &all);
    assert_eq!(armed.len(), 1);
    let c = armed.values().next().unwrap();
    assert_eq!((c.start, c.legs[0].pool, c.legs[1].pool), (USDC, UNI, SLIP));
}
