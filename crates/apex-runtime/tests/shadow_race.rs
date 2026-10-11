//! R25: the race ledger never blocks capture, says what happened, and the
//! nonce tripwire fires on any move.

use alloy_primitives::{B256, U256};
use apex_runtime::plane::{Decline, Handled};
use apex_runtime::shadow::funnel::decline_label;
use apex_runtime::shadow::ledger::{append_line, daily_path};
use apex_runtime::shadow::nonce_moved;
use apex_runtime::shadow::queue::{test_proposal, Pending};
use apex_runtime::shadow::race::{outcome_of, race_line, RaceLine, RaceOutcome, RaceTrigger, RaceWriter};
use apex_state::feed::event::EventKind;
use apex_state::Ordinal;
use apex_types::ids::{ChainId, TicketId};
use apex_types::ticket::{TerminalFailure, TicketOutcome, TicketStatus};
use apex_types::time::UnixNanos;
use std::sync::Arc;

fn line(n: u8) -> RaceLine {
    RaceLine {
        route_hash: format!("0x{n:064x}"),
        net_hint_wei: Some("1000".into()),
        trigger: RaceTrigger { block: 52_342_205, log_index: 51, tx: format!("0x{n:064x}") },
        observed_ns: 1,
        received_ns: 2,
        proposed_ns: 3,
        picked_ns: 4,
        handled_ns: 5,
        outcome: RaceOutcome::Stale,
        ticket: None,
    }
}

/// A full channel drops the line and counts it; capture never waits.
#[test]
fn a_full_race_channel_drops_and_counts() {
    let (w, _rx) = RaceWriter::new(2);
    for i in 0..5 {
        w.record(line(i));
    }
    assert_eq!((w.written(), w.dropped()), (2, 3));
}

#[test]
fn a_race_line_serializes_its_trigger_and_stages() {
    let v = serde_json::to_value(line(7)).unwrap();
    assert_eq!(v["trigger"]["block"], 52_342_205);
    assert_eq!(v["trigger"]["log_index"], 51);
    assert_eq!(v["received_ns"], 2);
    assert_eq!(v["outcome"], "stale");
    let failed = RaceLine { outcome: RaceOutcome::Failed { class: "SIM_FAIL/MinOutNotMet".into() }, ticket: Some(263), ..line(7) };
    let v = serde_json::to_value(failed).unwrap();
    assert_eq!(v["outcome"]["failed"]["class"], "SIM_FAIL/MinOutNotMet");
    assert_eq!(v["ticket"], 263);
}

/// Each stage's time lands in its own field, and the trigger is the swap's
/// position and transaction: the analyzer joins on all of them.
#[test]
fn a_handled_proposal_becomes_a_line_with_each_stage_in_place() {
    let mut ev = apex_search::frontier::doc_event();
    ev.at = Ordinal::confirmed(52_342_205, 303, 51);
    ev.observed_at = UnixNanos(10);
    ev.kind = EventKind::PendingSwap { target: B256::repeat_byte(0x7a), pools: vec![], notional_usd: None };
    let mut proposal = test_proposal(B256::repeat_byte(0x11), &[]);
    // Engine C dates a proposal by its event: `found_at` is not when the
    // search returned, and the line must not take it for that.
    proposal.found_at = UnixNanos(10);
    proposal.net_hint = Some(U256::from(1_798u64));
    let p = Pending { event: Arc::new(ev), proposal, received: UnixNanos(20), proposed: UnixNanos(30) };

    let l = race_line(&p, UnixNanos(40), UnixNanos(50), (RaceOutcome::Dispatched, Some(263)));
    assert_eq!(l.trigger, RaceTrigger { block: 52_342_205, log_index: 51, tx: format!("0x{}", "7a".repeat(32)) });
    assert_eq!((l.observed_ns, l.received_ns, l.proposed_ns, l.picked_ns, l.handled_ns), (10, 20, 30, 40, 50));
    assert_eq!(l.route_hash, format!("0x{}", "11".repeat(32)));
    assert_eq!(l.net_hint_wei.as_deref(), Some("1798"));
    assert_eq!((l.outcome, l.ticket), (RaceOutcome::Dispatched, Some(263)));

    let block = Pending { event: Arc::new(apex_search::frontier::doc_event()), ..p };
    assert_eq!(race_line(&block, UnixNanos(0), UnixNanos(0), (RaceOutcome::Stale, None)).trigger.tx, "", "an event that names no transaction");
}

/// The outcome is the funnel's label, and the ticket rides along when there
/// was one.
#[test]
fn an_outcome_says_what_became_of_the_proposal() {
    let closed = |o| Handled::Closed { ticket: TicketId(263), outcome: Box::new(o) };
    let shadow = TicketOutcome::ShadowDispatched { stage: TicketStatus::Acknowledged, at: UnixNanos(1), in_time: true, waived: vec![] };
    assert_eq!(outcome_of(&closed(shadow)), (RaceOutcome::Dispatched, Some(263)));
    let failed = TicketOutcome::ExplicitFailure {
        code: TerminalFailure::RiskRejected { rule: "r".into() },
        at: UnixNanos(1),
        state: Box::new(apex_search::frontier::doc_event().fingerprint),
        cause: "c".into(),
    };
    assert_eq!(outcome_of(&closed(failed)), (RaceOutcome::Failed { class: "failed/RiskRejected".into() }, Some(263)));
    let low = Decline::NoProfitableSize;
    assert_eq!(outcome_of(&Handled::Declined(low.clone())), (RaceOutcome::Declined { reason: decline_label(&low) }, None));
    let again = Handled::Redelivered { chain: ChainId::BASE, at: Ordinal::confirmed(1, 0, 0) };
    assert_eq!(outcome_of(&again), (RaceOutcome::Redelivered, None));
}

/// One file per UTC day: the analyzer globs them.
#[test]
fn ledgers_rotate_by_utc_day() {
    let p = daily_path(std::path::Path::new("var/apex/race.jsonl"), 1_791_743_507_000_000_000);
    assert_eq!(p, std::path::PathBuf::from("var/apex/race-2026-10-11.jsonl"), "18:31:47 UTC on the 11th");
    // The last nanosecond of a day and the first of the next.
    let midnight = 1_791_763_200_000_000_000;
    assert_eq!(daily_path(std::path::Path::new("f.jsonl"), midnight - 1), std::path::PathBuf::from("f-2026-10-11.jsonl"));
    assert_eq!(daily_path(std::path::Path::new("f.jsonl"), midnight), std::path::PathBuf::from("f-2026-10-12.jsonl"));
    assert_eq!(daily_path(std::path::Path::new("f.jsonl"), 1_709_164_800_000_000_000), std::path::PathBuf::from("f-2024-02-29.jsonl"));
}

/// Lines append to the day's file, one JSON object per line, in a directory
/// made on first use.
#[test]
fn a_ledger_line_appends_to_its_days_file() {
    let dir = std::env::temp_dir().join(format!("apex-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let base = dir.join("nested").join("race.jsonl");
    let at = 1_791_743_507_000_000_000;
    append_line(&base, at, &line(1)).unwrap();
    append_line(&base, at + 1, &line(2)).unwrap();
    let text = std::fs::read_to_string(daily_path(&base, at)).unwrap();
    let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["route_hash"], format!("0x{:064x}", 2));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A shadow never broadcasts, so the signer's nonce cannot move.
#[test]
fn the_tripwire_fires_on_any_move_of_the_nonce() {
    assert!(!nonce_moved(None, 7), "no boot reading yet");
    assert!(!nonce_moved(Some(7), 7));
    assert!(nonce_moved(Some(7), 8));
    assert!(nonce_moved(Some(7), 6));
}
