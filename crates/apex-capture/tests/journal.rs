//! Task 6.1 — the journal half. §17.5, §46.1, B-6.

include!("fixtures.rs");

use apex_capture::journal::{FileJournal, InMemoryJournal, Journal, JournalEntry};
use apex_types::ids::TicketId;
use apex_types::ticket::{TerminalFailure, TicketOutcome};
use std::io::Write;

fn admitted(id: u64) -> JournalEntry {
    JournalEntry::Admitted {
        id: TicketId(id),
        at: UnixNanos(1_700_000_000_000_000_000),
        ticket: Box::new(ticket()),
    }
}

fn advanced(id: u64, to: TicketStatus) -> JournalEntry {
    JournalEntry::Advanced { id: TicketId(id), at: UnixNanos(1_700_000_000_000_000_001), to }
}

fn closed(id: u64) -> JournalEntry {
    JournalEntry::Closed {
        id: TicketId(id),
        at: UnixNanos(1_700_000_000_000_000_002),
        outcome: Box::new(TicketOutcome::ExplicitFailure {
            code: TerminalFailure::Abandoned { at_status: TicketStatus::Signed },
            at: UnixNanos(1_700_000_000_000_000_002),
            state: Box::new(fingerprint()),
            cause: "test".to_string(),
        }),
    }
}

/// §17.5's boundary, stated as a table so the line is visible rather than
/// inferred. Before `Authorized` a ticket carries no capital risk.
#[test]
fn the_durable_write_boundary_is_authorized() {
    assert!(!admitted(1).requires_durable_write());
    for s in [TicketStatus::Observed, TicketStatus::Reserved, TicketStatus::Exacting, TicketStatus::Simulated] {
        assert!(!advanced(1, s).requires_durable_write(), "{s:?} should buffer");
    }
    for s in [
        TicketStatus::Authorized,
        TicketStatus::Signed,
        TicketStatus::Dispatching,
        TicketStatus::Acknowledged,
        TicketStatus::Preconfirmed,
        TicketStatus::Included,
        TicketStatus::Finalized,
        TicketStatus::Reconciled,
    ] {
        assert!(advanced(1, s).requires_durable_write(), "{s:?} should sync");
    }
    // A terminal outcome always syncs, whatever stage it closed at. Losing one
    // turns a classified failure into an unclassified one.
    assert!(closed(1).requires_durable_write());
}

#[test]
fn a_file_journal_syncs_only_at_the_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let j = FileJournal::open(dir.path().join("tickets.jsonl")).unwrap();
    j.append(&admitted(1)).unwrap();
    j.append(&advanced(1, TicketStatus::Reserved)).unwrap();
    j.append(&advanced(1, TicketStatus::Simulated)).unwrap();
    assert_eq!(j.sync_count(), 0, "nothing at risk yet");
    j.append(&advanced(1, TicketStatus::Authorized)).unwrap();
    assert_eq!(j.sync_count(), 1);
    j.append(&closed(1)).unwrap();
    assert_eq!(j.sync_count(), 2);
}

#[test]
fn a_file_journal_replays_what_it_wrote_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tickets.jsonl");
    let written = vec![admitted(1), advanced(1, TicketStatus::Authorized), admitted(2), closed(1)];
    {
        let j = FileJournal::open(&path).unwrap();
        for e in &written {
            j.append(e).unwrap();
        }
    }
    // Reopened, as recovery does.
    let j = FileJournal::open(&path).unwrap();
    assert_eq!(j.replay().unwrap(), written);
}

/// Appending to an existing journal must not truncate it -- recovery reads
/// across restarts, and `O_TRUNC` here would erase exactly the in-flight
/// tickets §46.1 requires reconciling.
#[test]
fn reopening_appends_rather_than_truncating() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tickets.jsonl");
    FileJournal::open(&path).unwrap().append(&admitted(1)).unwrap();
    FileJournal::open(&path).unwrap().append(&admitted(2)).unwrap();
    let ids: Vec<u64> =
        FileJournal::open(&path).unwrap().replay().unwrap().iter().map(|e| e.ticket_id().0).collect();
    assert_eq!(ids, vec![1, 2]);
}

/// The crash shape: a process killed mid-append leaves a half-written last
/// line. Everything before it is intact and must still be recovered, because
/// refusing to start would turn a routine crash into an outage.
#[test]
fn a_truncated_final_line_is_dropped_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tickets.jsonl");
    {
        let j = FileJournal::open(&path).unwrap();
        j.append(&admitted(1)).unwrap();
        j.append(&advanced(1, TicketStatus::Authorized)).unwrap();
    }
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(br#"{"Closed":{"id":{"0":1},"at":"#).unwrap();
    drop(f);

    let replayed = FileJournal::open(&path).unwrap().replay().unwrap();
    assert_eq!(replayed.len(), 2, "the two complete entries must survive");
}

/// Anywhere but the end means the file was rewritten or corrupted behind us.
/// Recovery decides what is still owed on-chain; guessing from a file it cannot
/// trust is how a ticket gets settled twice.
#[test]
fn a_corrupt_interior_line_refuses_to_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tickets.jsonl");
    {
        let j = FileJournal::open(&path).unwrap();
        j.append(&admitted(1)).unwrap();
    }
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"}{ not json\n").unwrap();
    }
    {
        let j = FileJournal::open(&path).unwrap();
        j.append(&closed(1)).unwrap();
    }
    let err = FileJournal::open(&path).unwrap().replay().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("line 2 of 3"), "got: {err}");
}

#[test]
fn the_in_memory_journal_agrees_with_the_file_one() {
    let dir = tempfile::tempdir().unwrap();
    let file = FileJournal::open(dir.path().join("t.jsonl")).unwrap();
    let mem = InMemoryJournal::new();
    let entries = vec![admitted(1), advanced(1, TicketStatus::Signed), closed(1)];
    for e in &entries {
        file.append(e).unwrap();
        mem.append(e).unwrap();
    }
    assert_eq!(file.replay().unwrap(), mem.replay().unwrap());
    assert_eq!(file.sync_count(), mem.sync_count(), "the test double must sync where the real one does");
}
