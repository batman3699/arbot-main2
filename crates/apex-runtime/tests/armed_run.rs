//! R26: `apex armed-eval`'s reading of the chain: logs into ordered effects on
//! the words the arms watch.

use alloy_primitives::{address, Address, B256};
use apex_chain::rpc::ws::RawLog;
use apex_runtime::armed::eval::Write;
use apex_runtime::armed::run::{effects_of, summary};
use apex_runtime::live::feed::{BURN, MINT, SWAP, SYNC};
use apex_runtime::live::inventory::Venue;
use ethers_core::types::U256;
use std::collections::BTreeMap;

const POOL: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const AERO: Address = address!("01784ef301d79e4b2df3a21ad9a536d4cf09a5ce");

/// A signed 256-bit ABI word.
fn word(v: i128) -> [u8; 32] {
    let mut w = if v < 0 { [0xff; 32] } else { [0; 32] };
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

fn log(address: Address, topics: Vec<B256>, data: Vec<[u8; 32]>, tx_index: u64, log_index: u64) -> RawLog {
    RawLog {
        pending: false,
        removed: false,
        address,
        topics,
        data: data.concat(),
        block_number: Some(100),
        transaction_hash: Some(B256::repeat_byte(tx_index as u8)),
        log_index: Some(log_index),
        transaction_index: Some(tx_index),
    }
}

/// A Swap sets the price word, the liquidity word and the tick; a Mint or Burn
/// moves liquidity over its range; a Sync sets both reserves; one
/// transaction's logs are one effect, in log order.
#[test]
fn logs_become_ordered_effects_on_the_watched_words() {
    let venues: BTreeMap<Address, Venue> = [(POOL, Venue::UniswapV3), (AERO, Venue::AerodromeV2)].into_iter().collect();
    let tick = |t: i32| B256::from(word(i128::from(t)));
    let swap = log(POOL, vec![SWAP, B256::ZERO, B256::ZERO], vec![word(-5), word(7), word(1 << 96), word(1_000), word(-12)], 4, 9);
    let mint = log(POOL, vec![MINT, B256::ZERO, tick(-60), tick(60)], vec![word(0), word(500), word(1), word(1)], 4, 10);
    let burn = log(POOL, vec![BURN, B256::ZERO, tick(-60), tick(60)], vec![word(200), word(1), word(1)], 6, 2);
    let sync = log(AERO, vec![SYNC], vec![word(11), word(22)], 6, 3);
    let mut removed = log(POOL, vec![BURN, B256::ZERO, tick(-60), tick(60)], vec![word(999), word(1), word(1)], 6, 4);
    removed.removed = true;
    let effects = effects_of(&[sync, burn, swap, removed, mint], &venues);
    assert_eq!(effects.len(), 2, "two transactions");
    assert_eq!((effects[0].tx_index, effects[1].tx_index), (4, 6), "chain order");
    let a = format!("{POOL:#x}");
    let b = format!("{AERO:#x}");
    assert_eq!(effects[0].writes, vec![
        Write::Set { address: a.clone(), slot: 0, value: U256::one() << 96 },
        Write::Set { address: a.clone(), slot: 4, value: U256::from(1_000u64) },
        Write::Tick { address: a.clone(), tick: -12 },
        Write::Liquidity { address: a.clone(), slot: 4, lower: -60, upper: 60, delta: 500 },
    ]);
    assert_eq!(effects[1].writes, vec![
        Write::Liquidity { address: a, slot: 4, lower: -60, upper: 60, delta: -200 },
        Write::Set { address: b.clone(), slot: 20, value: U256::from(11u64) },
        Write::Set { address: b, slot: 21, value: U256::from(22u64) },
    ], "the removed log is ignored");
}

/// One verdict line: the fields every verdict has, then `extra`.
fn verdict(arm_id: u64, fired: bool, extra: serde_json::Value) -> String {
    let mut v = serde_json::json!({
        "at_ns": 0, "arm_id": arm_id, "route_hash": "0xroute", "trigger_leg": 0, "rung": 1,
        "fired": fired, "shadowed": false, "base_fee_wei": "5000000", "per_f": [], "weth_usd": 2500.0,
    });
    v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    v.to_string()
}

fn per_f(f: u32, optimistic: bool, pessimistic: bool, net_wei: &str) -> serde_json::Value {
    serde_json::json!({
        "f_permille": f, "bid_wei_per_gas": "1",
        "landing": { "optimistic": optimistic, "pessimistic": pessimistic }, "net_wei": net_wei,
    })
}

/// The bid curve sums landed fires per day and bid fraction, in dollars at
/// each verdict's WETH price; a shadowed fire is not counted (its higher rung
/// is), a landed revert is; unfired arms split by what missed.
#[test]
fn the_summary_sums_the_bid_curve_and_splits_the_misses() {
    let dir = tempfile::tempdir().unwrap();
    let lines = [
        verdict(1, true, serde_json::json!({
            "trigger_met": true,
            "sim": { "ok": true, "gross_wei": "1", "gas_used": 1, "agrees": true },
            // 0.002 WETH and 0.001 WETH at $2,500.
            "per_f": [per_f(100, true, false, "2000000000000000"), per_f(500, true, true, "1000000000000000")],
        })),
        verdict(2, true, serde_json::json!({ "shadowed": true, "per_f": [per_f(100, true, true, "9000000000000000000")] })),
        verdict(3, false, serde_json::json!({ "trigger_met": false })),
        verdict(4, false, serde_json::json!({ "trigger_met": true })),
        verdict(5, false, serde_json::json!({})),
        verdict(6, true, serde_json::json!({
            "quote_error": "execution reverted: min out",
            "per_f": [per_f(100, true, false, "-50000000000000")],
        })),
    ];
    std::fs::write(dir.path().join("armed-verdicts-2026-10-13.jsonl"), lines.join("\n") + "\n").unwrap();
    std::fs::write(dir.path().join("armed-2026-10-13.jsonl"), verdict(9, true, serde_json::json!({}))).unwrap();

    let s = summary(dir.path()).unwrap();
    assert_eq!(s.arms_judged, 6, "the arms ledger beside it is not read");
    assert_eq!((s.unfired_trigger_unmet, s.unfired_band_missed), (1, 1), "a verdict without the field counts in neither");
    assert_eq!((s.simulated, s.sim_agreed), (1, 1));
    let day = &s.days["2026-10-13"];
    let f100 = &day[&100];
    assert_eq!((f100.fires, f100.wins_optimistic, f100.wins_pessimistic, f100.included_reverts), (2, 2, 0, 1));
    assert_eq!(f100.net_usd_optimistic, 5.0 - 0.125);
    let f500 = &day[&500];
    assert_eq!((f500.fires, f500.wins_optimistic, f500.wins_pessimistic), (1, 1, 1));
    assert_eq!((f500.net_usd_optimistic, f500.net_usd_pessimistic), (2.5, 2.5));
    assert_eq!(day.len(), 2);
}
