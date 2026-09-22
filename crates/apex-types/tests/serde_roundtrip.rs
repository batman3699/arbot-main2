//! Every persisted type must survive a round trip.
//!
//! Not ceremony: the ticket journal is the crash-recovery record (§16.8,
//! INV-39), and a type that cannot be read back is a ticket that cannot be
//! reconciled -- which is the precise failure §46.1 calls architecturally
//! defective ("no unclassified outcome").

use apex_types::ticket::{OpportunityTicket, TicketStatus};

mod fixtures {
    include!("fixtures.rs");
}

#[test]
fn a_ticket_survives_the_journal_round_trip() {
    let mut original = fixtures::ticket();
    original.advance(TicketStatus::Reserved).unwrap();
    original.advance(TicketStatus::Exacting).unwrap();

    let line = serde_json::to_string(&original).expect("serialise");
    let back: OpportunityTicket = serde_json::from_str(&line).expect("deserialise");

    assert_eq!(original, back);
    assert_eq!(back.status, TicketStatus::Exacting);
}

#[test]
fn deadlines_survive_as_wall_clock_not_process_local_instants() {
    // The reason time.rs does not use std::time::Instant: a deadline that
    // cannot cross a process boundary cannot be recovered after SIGKILL.
    let t = fixtures::ticket();
    let back: OpportunityTicket =
        serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();

    assert_eq!(back.dispatch_deadline, t.dispatch_deadline);
    assert!(back.dispatch_deadline.0 > 0, "a zero deadline would read as expired");
    assert!(back.is_past_deadline(apex_types::time::UnixNanos(u64::MAX)));
    assert!(!back.is_past_deadline(apex_types::time::UnixNanos(0)));
}
