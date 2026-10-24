//! Task 8.5 R36 — `apex validity-probe`'s predicate sets, exactly as sent:
//! one that can never hold, one that holds at once, one held past a
//! replacement, and the eight an arm carries, all true.

use alloy_primitives::Address;
use apex_runtime::validity_probe::probe_predicates;
use serde_json::{json, Value};

const SENDER: &str = "0xa1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const WETH: &str = "0x4200000000000000000000000000000000000006";
const PRICE: &str = "0xffffffffffffffffffffffffffffffffffffffff";
const LIQUIDITY: &str = "0xffffffffffffffffffffffffffffffff";

fn storage(slot: &str, mask: &str, op: &str, value: &str) -> Value {
    json!({"type": "storage", "params": {"address": WETH, "slot": slot, "mask": mask, "op": op, "value": value}})
}

fn block(op: &str, value: &str) -> Value {
    json!({"type": "block_number", "params": {"op": op, "value": value}})
}

/// 2^128: more than any balance.
fn never() -> Value {
    json!({"type": "balance", "params": {"address": SENDER, "op": ">=", "value": "0x100000000000000000000000000000000"}})
}

#[test]
fn the_probe_s_predicates_are_the_cases() {
    let cases = probe_predicates(100, Address::repeat_byte(0xa1));
    let names: Vec<&str> = cases.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["never", "now", "held", "eight"]);

    assert_eq!(cases[0].1, json!([never(), block("<=", "0x6a")]), "head 100 + 6");
    assert_eq!(cases[1].1, json!([block(">=", "0x64"), block("<=", "0x6a")]));
    assert_eq!(cases[2].1, json!([never(), block("<=", "0x7d")]), "head 100 + 25: inside the 60 s lifetime");

    let eight = json!([
        storage("0x0", PRICE, ">=", "0x0"),
        storage("0x0", PRICE, "<=", PRICE),
        storage("0x1", PRICE, ">=", "0x0"),
        storage("0x1", PRICE, "<=", PRICE),
        storage("0x2", PRICE, ">=", "0x0"),
        storage("0x2", PRICE, "<=", PRICE),
        storage("0x4", LIQUIDITY, ">=", "0x0"),
        block("<=", "0x6a"),
    ]);
    assert_eq!(cases[3].1, eight);
    let all = cases[3].1.as_array().unwrap();
    assert_eq!(all.len(), 8);
    assert_eq!(all.iter().filter(|p| p["type"] == "block_number").count(), 1);
}

/// The probe moves its sender's nonce. A sender armed-live's ledger already
/// knows would then sit where the ledger did not leave it, and the next boot
/// would trip on it.
#[test]
fn the_probe_refuses_a_sender_the_live_ledger_knows() {
    use apex_runtime::armed::live_ledger::{LiveLedger, LiveRecord};
    use apex_runtime::validity_probe::ledger_knows;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("armed-live.jsonl");
    assert!(!ledger_knows(&path, Address::repeat_byte(0xa1)).unwrap(), "no ledger yet");
    let (ledger, _) = LiveLedger::open(&path).unwrap();
    let skipped = LiveRecord::Skipped { arm_id: 1, sender: Address::repeat_byte(0xa1), reason: "budget".into(), at_ms: 1 };
    ledger.append(&skipped).unwrap();
    assert!(ledger_knows(&path, Address::repeat_byte(0xa1)).unwrap());
    assert!(!ledger_knows(&path, Address::repeat_byte(0xa2)).unwrap());
}
