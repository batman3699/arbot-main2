//! Task 8.5 R28: the canary's ledger, its limits and its halts
//! (`apex_runtime::canary`).

use alloy_primitives::{B256, U256};
use apex_runtime::canary::ledger::{Halt, Outcome, Record, Sent, Settled};
use apex_runtime::canary::limits::{Canary, Limits};
use apex_runtime::risk::Hold;
use apex_types::ids::{ChainId, StrategyId, VenueId};
use std::path::Path;

fn limits() -> Limits {
    Limits { loss_cap_wei: 1_000, max_consecutive_reverts: 3, max_consecutive_refusals: 2, max_landed: 4 }
}

fn sent(n: u8) -> Sent {
    Sent {
        ticket: u64::from(n),
        nonce: u64::from(n),
        tx: B256::repeat_byte(n),
        at_ms: 1,
        chain: ChainId::BASE,
        strategy: StrategyId(1),
        route_hash: B256::repeat_byte(0x44),
        venues: vec![VenueId(1)],
        gas_limit: 500_000,
        max_fee_per_gas: 20_000_000,
        max_priority_fee_per_gas: 1_000_000,
        expected_gross_wei: U256::from(900u64),
        expected_net_wei: 400,
        waived: vec!["route_authorization_valid".into()],
    }
}

fn landed(n: u8, net: i128) -> Outcome {
    Outcome {
        tx: B256::repeat_byte(n),
        at_ms: 2,
        settled: Settled::Landed {
            block: 1,
            gas_used: 400_000,
            effective_gas_price_wei: 1,
            l1_fee_wei: 0,
            gross_wei: net + 400_000,
            cost_wei: 400_000,
            net_wei: net,
        },
    }
}

fn reverted(n: u8, cost: u128) -> Outcome {
    Outcome {
        tx: B256::repeat_byte(n),
        at_ms: 2,
        settled: Settled::Reverted {
            block: 1,
            gas_used: 400_000,
            effective_gas_price_wei: 1,
            l1_fee_wei: 0,
            cost_wei: cost,
            class: "MinOutNotMet".into(),
            early: false,
        },
    }
}

fn open(dir: &Path) -> Canary {
    Canary::open(&dir.join("canary.jsonl"), &dir.join("canary.stop"), limits()).expect("opens")
}

#[test]
fn the_loss_cap_trips_at_its_threshold_and_not_one_short() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    c.sent(sent(1)).unwrap();
    c.settle(reverted(1, 999)).unwrap();
    assert_eq!(c.halted(), None, "999 of 1,000 lost");
    c.sent(sent(2)).unwrap();
    c.settle(reverted(2, 1)).unwrap();
    assert_eq!(c.halted(), Some(Halt::LossCap { lost_wei: 1_000 }));
}

#[test]
fn wins_do_not_offset_the_loss_cap() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    c.sent(sent(1)).unwrap();
    c.settle(landed(1, 10_000)).unwrap();
    c.sent(sent(2)).unwrap();
    c.settle(landed(2, -1_000)).unwrap();
    assert_eq!(c.halted(), Some(Halt::LossCap { lost_wei: 1_000 }), "a 10,000 win does not cover a 1,000 loss");
}

#[test]
fn reverts_in_a_row_trip_and_a_landing_resets_the_count() {
    let d = tempfile::tempdir().unwrap();
    let c = Canary::open(&d.path().join("l"), &d.path().join("s"), Limits { loss_cap_wei: u128::MAX, ..limits() }).unwrap();
    for n in 1..=2 {
        c.sent(sent(n)).unwrap();
        c.settle(reverted(n, 1)).unwrap();
    }
    c.sent(sent(3)).unwrap();
    c.settle(landed(3, 5)).unwrap();
    for n in 4..=5 {
        c.sent(sent(n)).unwrap();
        c.settle(reverted(n, 1)).unwrap();
    }
    assert_eq!(c.halted(), None, "two in a row after the landing");
    c.sent(sent(6)).unwrap();
    c.settle(reverted(6, 1)).unwrap();
    assert_eq!(c.halted(), Some(Halt::Reverts { in_a_row: 3 }));
}

#[test]
fn refusals_in_a_row_trip() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    for n in 1..=2 {
        c.sent(sent(n)).unwrap();
        c.settle(Outcome { tx: B256::repeat_byte(n), at_ms: 2, settled: Settled::Refused { detail: "-32605".into() } }).unwrap();
    }
    assert_eq!(c.halted(), Some(Halt::Refusals { in_a_row: 2 }));
}

#[test]
fn a_silent_send_halts_as_unresolved_and_stays_open() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    c.sent(sent(1)).unwrap();
    c.settle(Outcome { tx: B256::repeat_byte(1), at_ms: 2, settled: Settled::Silent { detail: "timed out".into() } }).unwrap();
    assert_eq!(c.halted(), Some(Halt::Unresolved { tx: B256::repeat_byte(1) }));
    assert_eq!(c.open_sends().len(), 1, "recovery must look for it");
}

#[test]
fn the_canary_is_done_after_its_last_landing() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    for n in 1..=4 {
        c.sent(sent(n)).unwrap();
        c.settle(landed(n, 5)).unwrap();
    }
    assert_eq!(c.halted(), Some(Halt::Done { landed: 4 }));
}

#[test]
fn a_halt_survives_a_restart_and_a_reset_clears_it() {
    let d = tempfile::tempdir().unwrap();
    {
        let c = open(d.path());
        c.sent(sent(1)).unwrap();
        c.settle(reverted(1, 1_000)).unwrap();
    }
    let again = open(d.path());
    assert_eq!(again.halted(), Some(Halt::LossCap { lost_wei: 1_000 }));
    assert!(again.reason().unwrap().contains("loss cap"));
    drop(again);
    // What scripts/canary-reset.sh appends.
    std::fs::OpenOptions::new()
        .append(true)
        .open(d.path().join("canary.jsonl"))
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"{\"kind\":\"reset\",\"at_ms\":3}\n"))
        .unwrap();
    let reset = open(d.path());
    assert_eq!(reset.halted(), None);
    assert_eq!(reset.report().totals.lost_wei, 0, "the cap counts from the reset");
}

#[test]
fn the_stop_file_holds_while_present_and_is_not_a_halt() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    assert_eq!(c.reason(), None);
    std::fs::write(d.path().join("canary.stop"), b"").unwrap();
    assert!(c.reason().unwrap().contains("stop file"));
    assert_eq!(c.halted(), None);
    std::fs::remove_file(d.path().join("canary.stop")).unwrap();
    assert_eq!(c.reason(), None);
}

#[test]
fn a_sent_record_is_on_disk_when_sent_returns() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    c.sent(sent(7)).unwrap();
    let text = std::fs::read_to_string(d.path().join("canary.jsonl")).unwrap();
    let r: Record = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(r, Record::Sent(sent(7)));
    assert_eq!(c.sent_since_boot(), 1);
}

/// Review focus 3: a crash mid-append leaves a torn last line, and nothing was
/// sent after it, because nothing is sent until the append returns. A torn line
/// anywhere else is damage, and the canary refuses to open over it.
#[test]
fn a_torn_last_line_is_ignored_and_a_torn_middle_line_refuses() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("canary.jsonl");
    let good = serde_json::to_string(&Record::Sent(sent(1))).unwrap();
    std::fs::write(&path, format!("{good}\n{{\"kind\":\"sen")).unwrap();
    assert_eq!(open(d.path()).open_sends().len(), 1);
    std::fs::write(&path, format!("{{\"kind\":\"sen\n{good}\n")).unwrap();
    assert!(Canary::open(&path, &d.path().join("s"), limits()).is_err());
    // A whole last line, newline and all, that does not parse is damage: a
    // torn append never reaches its newline, which is its last byte.
    std::fs::write(&path, format!("{good}\n{{\"kind\":\"sen\n")).unwrap();
    assert!(Canary::open(&path, &d.path().join("s"), limits()).is_err(), "a complete bad last line");
}
