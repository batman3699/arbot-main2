//! Task 8.5 R36: replay, worst-case reservations, UTC loss cap, and write-ahead JSONL.

use alloy_primitives::{Address, B256};
use apex_runtime::armed::live_ledger::{compact, nonce_views, Halt, LiveLedger, LiveRecord, LiveState};
use std::fs::OpenOptions;
use std::io::Write;
use tempfile::tempdir;

const OCT_24_UTC: u64 = 1_792_800_000_000;
const OCT_25_UTC: u64 = 1_792_886_400_000;

fn addr(byte: u8) -> Address {
    Address::from([byte; 20])
}

fn tx(byte: u8) -> B256 {
    B256::from([byte; 32])
}

fn submitting(sender: Address, nonce: u64, hash: B256, worst_case_wei: u128, at_ms: u64) -> LiveRecord {
    LiveRecord::Submitting {
        arm_id: u64::from(hash.as_slice()[0]),
        pools: [addr(0xa1), addr(0xb2)],
        sender,
        nonce,
        tx: hash,
        to_block: 123,
        gas_limit: 500_000,
        max_fee_per_gas: 10_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        worst_case_wei,
        at_ms,
        start: None,
    }
}

fn answered(hash: B256, accepted: Option<bool>, answer: &str, at_ms: u64) -> LiveRecord {
    LiveRecord::Answered { tx: hash, answer: answer.to_owned(), accepted, at_ms }
}

fn landed(sender: Address, nonce: u64, hash: B256, net_wei: i128, at_ms: u64) -> LiveRecord {
    LiveRecord::Landed {
        arm_id: u64::from(hash.as_slice()[0]),
        pools: [addr(0xa1), addr(0xb2)],
        sender,
        nonce,
        tx: hash,
        gross_wei: if net_wei > 0 { net_wei as u128 } else { 0 },
        paid_wei: if net_wei > 0 { net_wei as u128 } else { 0 },
        cost_wei: if net_wei < 0 { net_wei.unsigned_abs() } else { 0 },
        net_wei,
        at_ms,
        start: None,
        paid_start: None,
    }
}

#[test]
fn reservations_sum_the_worst_case_once_per_nonce() {
    let records = vec![
        submitting(addr(1), 7, tx(1), 3, OCT_24_UTC),
        submitting(addr(1), 7, tx(2), 5, OCT_24_UTC + 1),
        submitting(addr(2), 8, tx(3), 4, OCT_24_UTC + 2),
    ];
    let state = LiveState::from_records(&records, OCT_24_UTC + 3, 100);
    assert_eq!(state.reserved_wei, 9, "max(3, 5) for sender 1 nonce 7 plus 4 for sender 2 nonce 8");
    assert_eq!(state.pending.len(), 2);
    assert_eq!(state.pending[&(addr(1), 7)].len(), 2);
}

#[test]
fn a_refusal_releases_its_hash_but_a_silent_answer_keeps_its_reservation() {
    let records = vec![
        submitting(addr(1), 7, tx(1), 3, OCT_24_UTC),
        answered(tx(1), Some(false), "refused", OCT_24_UTC + 1),
        submitting(addr(2), 8, tx(2), 5, OCT_24_UTC + 2),
        answered(tx(2), None, "silent", OCT_24_UTC + 3),
    ];
    let state = LiveState::from_records(&records, OCT_24_UTC + 4, 100);
    assert_eq!(state.reserved_wei, 5);
    assert!(!state.pending.contains_key(&(addr(1), 7)));
    let still_pending = &state.pending[&(addr(2), 8)];
    assert_eq!(still_pending.len(), 1);
    assert_eq!(still_pending[0].accepted, None);
}

#[test]
fn a_landing_or_expiry_ends_every_submission_at_that_nonce() {
    let records = vec![
        submitting(addr(1), 7, tx(1), 3, OCT_24_UTC),
        submitting(addr(1), 7, tx(2), 5, OCT_24_UTC + 1),
        landed(addr(1), 7, tx(1), 2, OCT_24_UTC + 2),
        submitting(addr(2), 8, tx(3), 4, OCT_24_UTC + 3),
        LiveRecord::Expired { sender: addr(2), nonce: 8, tx: None, at_ms: OCT_24_UTC + 4 },
    ];
    let state = LiveState::from_records(&records, OCT_24_UTC + 5, 100);
    assert!(state.pending.is_empty());
    assert_eq!(state.reserved_wei, 0);
    assert_eq!(state.day_net, 2);
}

/// A renewal at a nonce whose earlier transaction's bound has passed: the
/// earlier one can no longer land, and goes alone.
#[test]
fn an_expiry_of_one_hash_keeps_the_rest_of_its_nonce() {
    let records = vec![
        submitting(addr(1), 7, tx(1), 5, OCT_24_UTC),
        submitting(addr(1), 7, tx(2), 3, OCT_24_UTC + 1),
        LiveRecord::Expired { sender: addr(1), nonce: 7, tx: Some(tx(1)), at_ms: OCT_24_UTC + 2 },
    ];
    let state = LiveState::from_records(&records, OCT_24_UTC + 3, 100);
    assert_eq!(state.pending[&(addr(1), 7)].iter().map(|p| p.tx).collect::<Vec<_>>(), vec![tx(2)]);
    assert_eq!(state.reserved_wei, 3);

    let mut all = records;
    all.push(LiveRecord::Expired { sender: addr(1), nonce: 7, tx: Some(tx(2)), at_ms: OCT_24_UTC + 4 });
    assert!(LiveState::from_records(&all, OCT_24_UTC + 5, 100).pending.is_empty(), "the last one empties the nonce");
}

/// Lines written before per-hash expiry name no hash, and end the nonce.
#[test]
fn an_expiry_without_a_hash_reads_as_the_whole_nonce() {
    let line = r#"{"kind":"expired","sender":"0x0101010101010101010101010101010101010101","nonce":8,"at_ms":1}"#;
    let record: LiveRecord = serde_json::from_str(line).unwrap();
    assert_eq!(record, LiveRecord::Expired { sender: addr(1), nonce: 8, tx: None, at_ms: 1 });
}

#[test]
fn the_budget_counts_realized_loss_and_reserved_exposure_together() {
    let records = vec![
        landed(addr(1), 7, tx(1), -4, OCT_24_UTC + 1),
        submitting(addr(2), 8, tx(2), 3, OCT_24_UTC + 2),
    ];
    let state = LiveState::from_records(&records, OCT_24_UTC + 3, 10);
    assert_eq!(state.day_net, -4);
    assert_eq!(state.reserved_wei, 3);
    assert!(state.admits(3, 10), "4 realized loss + 3 reserved + 3 new = 10");
    assert!(!state.admits(4, 10), "4 realized loss + 3 reserved + 4 new = 11");
}

#[test]
fn the_loss_cap_halts_for_the_utc_day_and_expires_on_rollover() {
    let records = vec![
        landed(addr(1), 7, tx(1), -11, OCT_24_UTC + 1),
        LiveRecord::Halted { reason: Halt::LossCap { day: "2026-10-24".into() }, at_ms: OCT_24_UTC + 2 },
    ];
    let before_midnight = LiveState::from_records(&records, OCT_25_UTC - 1, 10);
    assert_eq!(before_midnight.halted, Some(Halt::LossCap { day: "2026-10-24".into() }));
    assert_eq!(before_midnight.day_net, -11);

    let after_midnight = LiveState::from_records(&records, OCT_25_UTC, 10);
    assert_eq!(after_midnight.halted, None);
    assert_eq!(after_midnight.day, "2026-10-25");
    assert_eq!(after_midnight.day_net, 0, "yesterday's P&L is not today's budget usage");
}

#[test]
fn a_tripwire_holds_until_reset() {
    let records = vec![
        LiveRecord::Halted {
            reason: Halt::Tripwire { sender: addr(1), detail: "nonce advanced without our hash".into() },
            at_ms: OCT_24_UTC,
        },
    ];
    let halted = LiveState::from_records(&records, OCT_24_UTC + 1, 10);
    assert!(matches!(halted.halted, Some(Halt::Tripwire { sender, .. }) if sender == addr(1)));

    let mut reset = records;
    reset.push(LiveRecord::Reset { at_ms: OCT_24_UTC + 2 });
    assert_eq!(LiveState::from_records(&reset, OCT_24_UTC + 3, 10).halted, None);
}

/// The live worker keeps its state by applying each record as it writes it,
/// and moving to the next UTC day as the clock does: the same state a replay
/// at that moment would build.
#[test]
fn applying_records_one_at_a_time_matches_the_replay_across_midnight() {
    let records = vec![
        submitting(addr(1), 7, tx(1), 3, OCT_24_UTC),
        answered(tx(1), Some(true), "accepted", OCT_24_UTC + 1),
        submitting(addr(2), 8, tx(2), 4, OCT_24_UTC + 2),
        landed(addr(2), 8, tx(2), -11, OCT_24_UTC + 3),
        LiveRecord::Halted { reason: Halt::LossCap { day: "2026-10-24".into() }, at_ms: OCT_24_UTC + 4 },
        LiveRecord::Halted { reason: Halt::Tripwire { sender: addr(3), detail: "x".into() }, at_ms: OCT_24_UTC + 5 },
    ];
    let mut state = LiveState::new(OCT_24_UTC);
    for record in &records {
        state.apply(record, 10);
    }
    assert_eq!(state, LiveState::from_records(&records, OCT_24_UTC + 6, 10));
    assert_eq!(state.halted, Some(Halt::LossCap { day: "2026-10-24".into() }));

    state.roll(OCT_25_UTC);
    assert_eq!(state, LiveState::from_records(&records, OCT_25_UTC, 10));
    assert!(matches!(state.halted, Some(Halt::Tripwire { .. })), "the tripwire outlives the day");
    assert_eq!((state.day_net, state.reserved_wei), (0, 3), "the day's net restarts; the pending nonce is still held");
}

#[test]
fn a_crash_after_landed_but_before_halted_still_reconstructs_the_loss_cap() {
    // Task 10 syncs Landed before appending Halted. Replay must conservatively
    // recover the stop if a crash occurs between these two writes.
    let records = vec![landed(addr(1), 7, tx(1), -10, OCT_24_UTC + 1)];
    let state = LiveState::from_records(&records, OCT_24_UTC + 2, 10);
    assert_eq!(state.halted, Some(Halt::LossCap { day: "2026-10-24".into() }));
}

#[test]
fn a_torn_last_line_is_removed_before_the_next_append() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("armed-live.jsonl");
    let (ledger, records) = LiveLedger::open(&path).unwrap();
    assert!(records.is_empty());
    let first = submitting(addr(1), 7, tx(1), 3, OCT_24_UTC);
    ledger.append(&first).unwrap();
    drop(ledger);

    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"kind\":\"submitting\",\"tx\":").unwrap();
    file.sync_data().unwrap();
    drop(file);

    let (ledger, recovered) = LiveLedger::open(&path).unwrap();
    assert_eq!(recovered, vec![first.clone()], "whole records survive; the torn tail is discarded");
    ledger.append(&answered(tx(1), None, "silent", OCT_24_UTC + 1)).unwrap();
    drop(ledger);

    let (_ledger, recovered_again) = LiveLedger::open(&path).unwrap();
    assert_eq!(recovered_again, vec![first, answered(tx(1), None, "silent", OCT_24_UTC + 1)]);
}

#[test]
fn malformed_newline_terminated_records_fail_closed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("armed-live.jsonl");
    std::fs::write(&path, b"{not-json}\n").unwrap();
    let error = match LiveLedger::open(&path) {
        Err(error) => error,
        Ok(_) => panic!("malformed complete ledger record must be rejected"),
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("line 1"));
}

#[test]
fn all_record_variants_round_trip_and_wei_are_decimal_strings() {
    let records = vec![
        LiveRecord::Submitting {
            arm_id: 9,
            pools: [addr(0xa1), addr(0xb2)],
            sender: addr(1),
            nonce: 7,
            tx: tx(1),
            to_block: 150,
            gas_limit: 500_000,
            max_fee_per_gas: u128::MAX,
            max_priority_fee_per_gas: 20_000_000_000,
            worst_case_wei: u128::MAX,
            at_ms: OCT_24_UTC,
            start: None,
        },
        answered(tx(1), None, "network timeout", OCT_24_UTC + 1),
        landed(addr(1), 7, tx(1), -123, OCT_24_UTC + 2),
        LiveRecord::Expired { sender: addr(2), nonce: 8, tx: None, at_ms: OCT_24_UTC + 3 },
        LiveRecord::Skipped { arm_id: 10, sender: addr(2), reason: "budget".into(), at_ms: OCT_24_UTC + 4 },
        LiveRecord::Halted {
            reason: Halt::Tripwire { sender: addr(2), detail: "unexpected nonce".into() },
            at_ms: OCT_24_UTC + 5,
        },
        LiveRecord::Reset { at_ms: OCT_24_UTC + 6 },
        LiveRecord::Abandoned { sender: addr(3), nonce: 9, worst_case_wei: 5, at_ms: OCT_24_UTC + 7 },
    ];
    for record in records {
        let json = serde_json::to_value(&record).unwrap();
        let decoded: LiveRecord = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(decoded, record);
        if let LiveRecord::Submitting { worst_case_wei, max_fee_per_gas, .. } = record {
            assert_eq!(json["worst_case_wei"], serde_json::Value::String(worst_case_wei.to_string()));
            assert_eq!(json["max_fee_per_gas"], serde_json::Value::String(max_fee_per_gas.to_string()));
        }
    }
}

/// A batch stamped before midnight, applied after a head rolled the day
/// over, must not take the state back to a day with no loss on it.
#[test]
fn the_day_never_rolls_back() {
    let records = vec![landed(addr(1), 7, tx(1), -11, OCT_25_UTC + 1)];
    let mut state = LiveState::from_records(&records, OCT_25_UTC + 2, 10);
    state.roll(OCT_25_UTC - 1);
    assert_eq!((state.day.as_str(), state.day_net), ("2026-10-25", -11));
    assert_eq!(state.halted, Some(Halt::LossCap { day: "2026-10-25".into() }));
}

/// A nonce the chain passed without a receipt among ours: what happened to
/// our transaction is unknown, so its worst case is counted as spent.
#[test]
fn an_abandoned_nonce_is_charged_its_worst_case() {
    let records = vec![
        submitting(addr(1), 7, tx(1), 4, OCT_24_UTC),
        LiveRecord::Abandoned { sender: addr(1), nonce: 7, worst_case_wei: 4, at_ms: OCT_24_UTC + 1 },
    ];
    let state = LiveState::from_records(&records, OCT_24_UTC + 2, 10);
    assert!(state.pending.is_empty());
    assert_eq!((state.reserved_wei, state.day_net), (0, -4));
}

#[test]
fn a_reset_re_baselines_every_nonce_view() {
    let mut records = vec![
        submitting(addr(1), 7, tx(1), 4, OCT_24_UTC),
        LiveRecord::Expired { sender: addr(1), nonce: 7, tx: Some(tx(1)), at_ms: OCT_24_UTC + 1 },
        landed(addr(2), 3, tx(2), 1, OCT_24_UTC + 2),
    ];
    assert_eq!(nonce_views(&records), [(addr(1), 7), (addr(2), 4)].into_iter().collect());
    records.push(LiveRecord::Abandoned { sender: addr(2), nonce: 4, worst_case_wei: 1, at_ms: OCT_24_UTC + 3 });
    assert_eq!(nonce_views(&records), [(addr(1), 7)].into_iter().collect(), "an abandoned nonce leaves no view");
    records.push(LiveRecord::Reset { at_ms: OCT_24_UTC + 4 });
    assert!(nonce_views(&records).is_empty(), "after a reset every idle sender is taken at the chain's nonce");
}

/// A day of renewals: what a replay needs is a few records a sender, the
/// pending hashes', and every landing and halt.
fn a_busy_day() -> Vec<LiveRecord> {
    // addr 1, nonce 7: a renewal expired by hash, one refused, one pending.
    let mut r = vec![
        submitting(addr(1), 7, tx(1), 3, OCT_24_UTC),
        answered(tx(1), Some(true), "accepted", OCT_24_UTC + 1),
        LiveRecord::Expired { sender: addr(1), nonce: 7, tx: Some(tx(1)), at_ms: OCT_24_UTC + 2 },
        submitting(addr(1), 7, tx(2), 3, OCT_24_UTC + 3),
        answered(tx(2), Some(false), "underpriced", OCT_24_UTC + 4),
    ];
    r.extend((0..40u8).map(|i| LiveRecord::Skipped { arm_id: u64::from(i), sender: addr(1), reason: "budget".into(), at_ms: OCT_24_UTC + 5 }));
    r.extend([
        submitting(addr(1), 7, tx(3), 5, OCT_24_UTC + 6),
        answered(tx(3), Some(true), "accepted", OCT_24_UTC + 7),
        // addr 2: landed yesterday, then landed today at the next nonce.
        submitting(addr(2), 3, tx(4), 2, OCT_24_UTC + 8),
        landed(addr(2), 3, tx(4), -2, OCT_24_UTC + 9),
        submitting(addr(2), 4, tx(5), 2, OCT_25_UTC + 1),
        landed(addr(2), 4, tx(5), 6, OCT_25_UTC + 2),
        // addr 3: expired whole; addr 4: refused; addr 5: silent, pending.
        submitting(addr(3), 9, tx(6), 1, OCT_25_UTC + 3),
        LiveRecord::Expired { sender: addr(3), nonce: 9, tx: None, at_ms: OCT_25_UTC + 4 },
        submitting(addr(4), 2, tx(7), 1, OCT_25_UTC + 5),
        answered(tx(7), Some(false), "insufficient funds", OCT_25_UTC + 6),
        submitting(addr(5), 1, tx(8), 4, OCT_25_UTC + 7),
        answered(tx(8), None, "timeout", OCT_25_UTC + 8),
        // A tripwire and its reset, then a new tripwire still standing.
        LiveRecord::Halted { reason: Halt::Tripwire { sender: addr(6), detail: "x".into() }, at_ms: OCT_25_UTC + 9 },
        LiveRecord::Reset { at_ms: OCT_25_UTC + 10 },
        submitting(addr(6), 5, tx(9), 1, OCT_25_UTC + 11),
        LiveRecord::Abandoned { sender: addr(6), nonce: 5, worst_case_wei: 1, at_ms: OCT_25_UTC + 12 },
        LiveRecord::Halted { reason: Halt::Tripwire { sender: addr(6), detail: "y".into() }, at_ms: OCT_25_UTC + 13 },
    ]);
    r
}

#[test]
fn compaction_keeps_the_state_and_the_nonce_views() {
    let all = a_busy_day();
    let now = OCT_25_UTC + 20;
    let kept = compact(&all, now, 100).expect("a compaction that replays the same");
    assert!(kept.len() < all.len() / 2, "{} of {}", kept.len(), all.len());
    assert!(!kept.iter().any(|r| matches!(r, LiveRecord::Skipped { .. })));
    assert_eq!(LiveState::from_records(&kept, now, 100), LiveState::from_records(&all, now, 100));
    assert_eq!(nonce_views(&kept), nonce_views(&all));
    assert_eq!(kept.iter().filter(|r| matches!(r, LiveRecord::Landed { .. })).count(), 2, "every landing stays: the census reads them");
}

/// The daily restart compacts a ledger past its threshold, and keeps the
/// original beside it, byte for byte.
#[test]
fn a_long_ledger_is_compacted_and_the_original_kept() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("armed-live.jsonl");
    let (ledger, _) = LiveLedger::open(&path).unwrap();
    for r in a_busy_day() {
        ledger.append(&r).unwrap();
    }
    drop(ledger);
    let before = std::fs::read(&path).unwrap();
    let now = OCT_25_UTC + 20;
    assert_eq!(LiveLedger::compact_file(&path, now, 100, 10_000).unwrap(), None, "under the threshold: untouched");
    let archive = LiveLedger::compact_file(&path, now, 100, 10).unwrap().expect("compacted");
    assert_eq!(std::fs::read(&archive).unwrap(), before);
    let (_, records) = LiveLedger::open(&path).unwrap();
    assert_eq!(records, compact(&a_busy_day(), now, 100).unwrap());
}
