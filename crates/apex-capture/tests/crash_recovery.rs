//! Task 6.2 — INV-39, §16.8, §57.1.4.
//!
//! "On restart/reconnect/replacement/provider failure, every outstanding
//! transaction is reconciled before new live dispatch."
//!
//! The kill is real. `src/bin/crash_victim.rs` is a separate process that puts
//! tickets in flight and then blocks forever; these tests `SIGKILL` it and read
//! the journal it left behind. An in-process simulation would only ever produce
//! the file shape its author already imagined, and the whole question is what a
//! journal looks like after a process stops existing between two writes.

include!("fixtures.rs");

use apex_capture::journal::{FileJournal, InMemoryJournal, Journal, JournalEntry};
use apex_capture::recover::{
    reconcile, scan, ChainOutcomeSource, Disposition, DispatchGate, RecoveryError,
};
use apex_capture::registry::TicketRegistry;
use apex_capture::SystemClock;
use apex_types::ids::TicketId;
use apex_types::pnl::{OptimizationLayer, PnlAttribution, UsdBounds};
use apex_types::cost::{TotalExecutionCost};
use apex_types::ticket::{TerminalFailure, TicketOutcome};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

// ------------------------------------------------------------- chain doubles

/// Answers as though every possibly-landed ticket reverted. A *decision*, which
/// is all recovery needs -- the point under test is that one is reached, not
/// which one.
struct AlwaysReverted;

impl ChainOutcomeSource for AlwaysReverted {
    fn resolve(&self, t: &OpportunityTicket) -> Result<TicketOutcome, String> {
        Ok(TicketOutcome::ExplicitFailure {
            code: TerminalFailure::Reverted {
                revert_class: apex_types::sim::RevertClass::Unknown,
                data: Vec::new(),
            },
            at: UnixNanos(1_700_000_000_000_000_009),
            state: Box::new(t.state_fingerprint.clone()),
            cause: "receipt shows a revert".to_string(),
        })
    }
}

struct AlwaysLanded;

impl ChainOutcomeSource for AlwaysLanded {
    fn resolve(&self, t: &OpportunityTicket) -> Result<TicketOutcome, String> {
        Ok(TicketOutcome::Success {
            stage: TicketStatus::Included,
            realized: Box::new(PnlAttribution {
                ticket_id: t.ticket_id,
                chain: t.chain_id,
                strategy: t.strategy,
                venues: Vec::new(),
                route_hash: t.route_commitment.route_hash,
                optimization_layers: vec![OptimizationLayer::SinglePath],
                gross_profit: 2_000,
                realized_cost: TotalExecutionCost {
                    l2_execution_fee: 400,
                    l1_data_fee: 300,
                    priority_fee: 20,
                    builder_payment: 0,
                    sequencer_payment: 0,
                    flash_fee: 40,
                    dex_fees: 6,
                    expected_failure_cost: 0,
                    calldata_bytes: 1_200,
                    compressed_data_estimate: 700,
                    gas_limit: GasLimit(500_000),
                    gas_used_distribution: GasDistribution {
                        p50: GasUsed(300_000),
                        p90: GasUsed(330_000),
                        p99: GasUsed(360_000),
                        max_observed: GasUsed(380_000),
                    },
                },
                net_profit_token: 1_234,
                net_profit_usd_bounds: UsdBounds { low: 3.9, high: 4.1 },
            }),
        })
    }
}

/// The RPC is down. An outage must never be allowed to look like "it did not
/// land" -- that is how a ticket that moved money gets filed as a miss.
struct ChainDown;

impl ChainOutcomeSource for ChainDown {
    fn resolve(&self, _t: &OpportunityTicket) -> Result<TicketOutcome, String> {
        Err("no provider answered".to_string())
    }
}

// ------------------------------------------------------------- the real kill

fn spawn_victim(journal: &Path, unsigned: usize, signed: usize) -> Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_crash_victim"))
        .arg(journal)
        .arg(unsigned.to_string())
        .arg(signed.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the victim");
    // Block until it says its tickets are in flight. Sleeping a fixed amount
    // instead would make the test's coverage a function of how loaded the
    // machine is.
    let out = child.stdout.take().expect("victim stdout");
    let mut line = String::new();
    BufReader::new(out).read_line(&mut line).expect("victim readiness line");
    assert!(line.starts_with("READY"), "victim said: {line:?}");
    child
}

fn kill_mid_flight(journal: &Path, unsigned: usize, signed: usize) {
    let mut child = spawn_victim(journal, unsigned, signed);
    child.kill().expect("SIGKILL");
    let status = child.wait().expect("reap");
    assert!(!status.success(), "the victim exited cleanly; it is supposed to be killed");
}

// ------------------------------------------------------------------- tests

/// **INV-39.** The restarted process refuses to dispatch until reconciliation
/// completes, and every journalled ticket ends terminal.
#[test]
fn boot_blocks_dispatch_until_reconciled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tickets.jsonl");
    kill_mid_flight(&path, 2, 3);

    // --- restart ---
    let journal = FileJournal::open(&path).unwrap();
    let gate = DispatchGate::shut();
    assert!(gate.permit().is_none(), "a fresh boot must not be able to dispatch");

    let found = scan(&journal).unwrap();
    assert_eq!(found.unreconciled(), 5, "five tickets were in flight");
    assert_eq!(found.tickets_closed, 0);
    assert_eq!(found.in_flight.iter().filter(|d| d.needs_the_chain()).count(), 3);
    assert!(gate.permit().is_none(), "scanning is not reconciling");

    let registry = TicketRegistry::new(Box::new(journal), Box::new(SystemClock));
    let proof =
        reconcile(&registry, &found, &AlwaysReverted, UnixNanos(1_700_000_000_000_000_010)).unwrap();
    assert_eq!(proof.tickets_reconciled(), 5);

    gate.open(proof);
    assert!(gate.permit().is_some(), "dispatch must resume once nothing is outstanding");

    for d in &found.in_flight {
        assert!(registry.outcome(d.id()).is_some(), "ticket {} left unresolved", d.id().0);
    }
    let m = registry.metrics();
    assert_eq!(m.tickets_live, 0);
    assert_eq!(m.tickets_restored, 5);
    assert_eq!(m.ticket_drop_count(), 0, "INV-02 across a crash");
}

/// Acceptance criterion 3: twenty consecutive kill cycles, each one recovering
/// the last one's leavings. The journal accumulates across all twenty, so cycle
/// twenty replays nineteen closed generations as well as its own live one --
/// which is the case where an off-by-one in the replay shows up.
#[test]
fn twenty_consecutive_kill_cycles_each_recover_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tickets.jsonl");

    let mut total_reconciled = 0usize;
    for cycle in 1..=20 {
        kill_mid_flight(&path, 1, 1);

        let journal = FileJournal::open(&path).unwrap();
        let found = scan(&journal).unwrap();
        assert_eq!(found.unreconciled(), 2, "cycle {cycle} found {:?}", found.unreconciled());
        assert_eq!(found.tickets_closed, (cycle - 1) * 2, "cycle {cycle} lost earlier closures");

        let registry = TicketRegistry::new(Box::new(journal), Box::new(SystemClock));
        let proof = reconcile(
            &registry,
            &found,
            &AlwaysReverted,
            UnixNanos(1_700_000_000_000_000_010 + cycle as u64),
        )
        .unwrap();
        total_reconciled += proof.tickets_reconciled();
        assert_eq!(registry.metrics().tickets_live, 0, "cycle {cycle} left something live");
    }
    assert_eq!(total_reconciled, 40);

    // Every ticket the journal ever admitted has exactly one Closed entry.
    let final_scan = scan(&FileJournal::open(&path).unwrap()).unwrap();
    assert_eq!(final_scan.unreconciled(), 0);
    assert_eq!(final_scan.tickets_seen, 40);
    assert_eq!(final_scan.tickets_closed, 40);
}

/// A ticket the previous run never signed cannot be on chain -- §17.1, "a
/// transition that is not recorded did not happen" -- so it is closed from local
/// knowledge. One that reached `Signed` is not, and the split is what keeps the
/// chain source off the path it cannot help with.
#[test]
fn the_signed_boundary_decides_who_needs_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tickets.jsonl");
    kill_mid_flight(&path, 4, 1);

    let found = scan(&FileJournal::open(&path).unwrap()).unwrap();
    let never: Vec<_> =
        found.in_flight.iter().filter(|d| matches!(d, Disposition::NeverSigned(_))).collect();
    let maybe: Vec<_> =
        found.in_flight.iter().filter(|d| matches!(d, Disposition::MaybeOnChain(_))).collect();
    assert_eq!(never.len(), 4);
    assert_eq!(maybe.len(), 1);
    assert!(never.iter().all(|d| d.ticket().status < TicketStatus::Signed));
    assert!(maybe.iter().all(|d| d.ticket().status >= TicketStatus::Signed));
}

/// **An RPC outage must not be able to look like "it did not land".** Recovery
/// fails, the gate stays shut, and the never-signed tickets are still closed --
/// so the outage leaves less outstanding than it found, rather than the same
/// amount and a longer outage.
#[test]
fn a_chain_outage_blocks_the_gate_but_still_makes_progress() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tickets.jsonl");
    kill_mid_flight(&path, 3, 2);

    let journal = FileJournal::open(&path).unwrap();
    let found = scan(&journal).unwrap();
    let registry = TicketRegistry::new(Box::new(journal), Box::new(SystemClock));
    let gate = DispatchGate::shut();

    let err = reconcile(&registry, &found, &ChainDown, UnixNanos(1_700_000_000_000_000_010))
        .expect_err("an unanswerable chain must not produce a proof");
    assert!(matches!(err, RecoveryError::ChainUnavailable { .. }), "got {err:?}");
    assert!(gate.permit().is_none(), "the gate opened without a proof");

    // The three that were never signed are closed; the two that may be on chain
    // are not, and are still the registry's responsibility.
    let m = registry.metrics();
    assert_eq!(m.tickets_terminal_failure, 3);
    assert_eq!(m.tickets_live, 2);
    assert_eq!(m.ticket_drop_count(), 0);
}

/// A landed ticket is reconciled as a success, not as an abandonment. Getting
/// this backwards tells the P&L ledger a trade never happened while the money
/// moved (§2.10).
#[test]
fn a_ticket_that_landed_is_reconciled_as_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tickets.jsonl");
    kill_mid_flight(&path, 0, 2);

    let journal = FileJournal::open(&path).unwrap();
    let found = scan(&journal).unwrap();
    let registry = TicketRegistry::new(Box::new(journal), Box::new(SystemClock));
    reconcile(&registry, &found, &AlwaysLanded, UnixNanos(1_700_000_000_000_000_010)).unwrap();

    assert_eq!(registry.metrics().tickets_terminal_success, 2);
    for d in &found.in_flight {
        assert!(registry.outcome(d.id()).unwrap().is_success());
    }
}

/// Ids are not reissued after a crash. The counter in a fresh registry starts at
/// 1 and has no idea what the dead run handed out; two tickets sharing an id is
/// how one outcome overwrites an unrelated ticket's.
#[test]
fn recovery_does_not_reissue_an_id_the_dead_run_used() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tickets.jsonl");
    kill_mid_flight(&path, 2, 1);

    let journal = FileJournal::open(&path).unwrap();
    let found = scan(&journal).unwrap();
    assert_eq!(found.max_ticket_id, 3);

    let registry = TicketRegistry::new(Box::new(journal), Box::new(SystemClock));
    reconcile(&registry, &found, &AlwaysReverted, UnixNanos(1_700_000_000_000_000_010)).unwrap();

    let fresh = registry.admit(ticket()).unwrap();
    assert!(fresh.0 > found.max_ticket_id, "reissued id {}", fresh.0);
    assert!(registry.outcome(TicketId(1)).is_some(), "and the old outcome survived");
}

/// The id-reuse guard has two halves and they must be tested apart.
///
/// Found by mutation: deleting `reserve_ids_through` from `reconcile` broke
/// nothing, because `restore` also raises the counter -- and deleting
/// `restore`'s raise broke nothing either, for the mirror reason. Each was
/// covering for the other, so neither was actually tested.
///
/// This is the case only `reserve_ids_through` can handle: the highest id in
/// the journal belongs to a ticket that was **already closed**, so it is never
/// restored and `restore` never sees it.
#[test]
fn the_highest_id_may_belong_to_an_already_closed_ticket() {
    let journal = std::sync::Arc::new(InMemoryJournal::new());
    {
        let reg = TicketRegistry::new(Box::new(SharedJournal(journal.clone())), Box::new(SystemClock));
        let a = reg.admit(ticket()).unwrap();
        let b = reg.admit(ticket()).unwrap();
        let c = reg.admit(ticket()).unwrap();
        assert_eq!((a.0, b.0, c.0), (1, 2, 3));
        // The LAST one closes; the first two are still in flight at the crash.
        reg.close(
            c,
            TicketOutcome::ExplicitFailure {
                code: TerminalFailure::CompetitorWon { observed_tx: None },
                at: UnixNanos(1),
                state: Box::new(fingerprint()),
                cause: "lost".to_string(),
            },
        )
        .unwrap();
        std::mem::forget(reg);
    }

    let found = scan(journal.as_ref()).unwrap();
    assert_eq!(found.max_ticket_id, 3);
    assert_eq!(found.unreconciled(), 2, "1 and 2 are outstanding; 3 is not");

    let reg = TicketRegistry::new(Box::new(SharedJournal(journal.clone())), Box::new(SystemClock));
    reconcile(&reg, &found, &AlwaysReverted, UnixNanos(10)).unwrap();

    let fresh = reg.admit(ticket()).unwrap();
    assert!(fresh.0 > 3, "id {} collides with the closed ticket 3", fresh.0);
}

/// And the mirror: `restore` used on its own, without `reconcile`, must still
/// stop the counter handing its id out again.
#[test]
fn restore_alone_reserves_the_id_it_restored() {
    let reg = TicketRegistry::in_memory();
    let mut t = ticket();
    t.ticket_id = TicketId(9);
    reg.restore(t).unwrap();

    let fresh = reg.admit(ticket()).unwrap();
    assert!(fresh.0 > 9, "admit reissued {} over a restored ticket", fresh.0);
}

/// A journal shared between two registries, which is what a restart is: the
/// same file, a new process. `Box<dyn Journal>` wants ownership, so this hands
/// each registry a handle to one underlying journal.
struct SharedJournal(std::sync::Arc<InMemoryJournal>);

impl Journal for SharedJournal {
    fn append(&self, e: &JournalEntry) -> std::io::Result<()> {
        self.0.append(e)
    }
    fn replay(&self) -> std::io::Result<Vec<JournalEntry>> {
        self.0.replay()
    }
    fn sync_count(&self) -> u64 {
        self.0.sync_count()
    }
}

// --------------------------------------------------- scan, without a process

#[test]
fn a_journal_that_closed_everything_has_nothing_to_reconcile() {
    let reg = TicketRegistry::in_memory();
    let id = reg.admit(ticket()).unwrap();
    reg.close(
        id,
        TicketOutcome::ExplicitFailure {
            code: TerminalFailure::CompetitorWon { observed_tx: None },
            at: UnixNanos(1),
            state: Box::new(fingerprint()),
            cause: "lost".to_string(),
        },
    )
    .unwrap();

    let found = scan(reg.journal()).unwrap();
    assert_eq!(found.unreconciled(), 0);
    assert_eq!(found.tickets_closed, 1);
}

/// A journal mentioning a ticket it never admitted was edited or concatenated
/// from two runs. Recovery refuses rather than reconstructing a ticket it never
/// saw -- it is about to decide what is still owed on-chain.
#[test]
fn an_orphan_entry_refuses_to_scan() {
    let j = InMemoryJournal::new();
    j.append(&JournalEntry::Advanced {
        id: TicketId(9),
        at: UnixNanos(1),
        to: TicketStatus::Signed,
    })
    .unwrap();
    assert!(matches!(scan(&j).unwrap_err(), RecoveryError::OrphanEntry(TicketId(9))));
}

/// One id naming two tickets cannot be reconciled: there is no way to say which
/// of them a `Closed` entry settled, and the dangerous reading is the plausible
/// one -- the second ticket inherits the first's closure and recovery walks past
/// a transaction that may be on chain. Found by the twenty-cycle test, where the
/// victim's fresh id counter restarted at 1 on every boot.
#[test]
fn a_journal_that_admits_one_id_twice_refuses_to_scan() {
    let j = InMemoryJournal::new();
    let entry = JournalEntry::Admitted {
        id: TicketId(4),
        at: UnixNanos(1),
        ticket: Box::new(ticket()),
    };
    j.append(&entry).unwrap();
    j.append(&JournalEntry::Closed {
        id: TicketId(4),
        at: UnixNanos(2),
        outcome: Box::new(TicketOutcome::ExplicitFailure {
            code: TerminalFailure::Abandoned { at_status: TicketStatus::Signed },
            at: UnixNanos(2),
            state: Box::new(fingerprint()),
            cause: "first".to_string(),
        }),
    })
    .unwrap();
    // Closed and then admitted again. Reading this as "ticket 4 is settled"
    // would silently discard the second one.
    j.append(&entry).unwrap();

    assert!(matches!(scan(&j).unwrap_err(), RecoveryError::DuplicateAdmission(TicketId(4))));
}

/// Replay reproduces the recorded history rather than re-adjudicating it: the
/// scan reports the last status the journal saw, whatever route it took there.
#[test]
fn scan_reports_the_last_recorded_status() {
    let reg = TicketRegistry::in_memory();
    let id = reg.admit(ticket()).unwrap();
    let mut g = reg.checkout(id).unwrap();
    g.advance(TicketStatus::Authorized).unwrap();
    g.advance(TicketStatus::Dispatching).unwrap();
    std::mem::forget(g); // leave it in flight, as a crash would

    let found = scan(reg.journal()).unwrap();
    assert_eq!(found.unreconciled(), 1);
    assert_eq!(found.in_flight[0].ticket().status, TicketStatus::Dispatching);
    assert!(found.in_flight[0].needs_the_chain());
}
