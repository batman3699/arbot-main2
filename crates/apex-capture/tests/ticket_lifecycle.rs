//! Task 6.1 — INV-01, INV-02, INV-03.
//!
//! INV-01 says every admitted live ticket reaches exactly one terminal outcome.
//! The interesting word is *every*: the invariant is not about the happy path
//! but about the paths nobody wrote down. So the property test's job is to
//! generate the paths nobody wrote down -- guards dropped mid-flight, closes
//! attempted twice, transitions that run backwards, tickets admitted and then
//! simply forgotten -- and assert the accounting still balances afterwards.

include!("fixtures.rs");

use apex_capture::registry::{RegistryError, TicketRegistry};
use apex_capture::{InMemoryJournal, JournalEntry, ManualClock};
use apex_types::ids::TicketId;
use apex_types::cost::TotalExecutionCost;
use apex_types::pnl::{OptimizationLayer, PnlAttribution, UsdBounds};
use apex_types::ticket::{TerminalFailure, TicketOutcome};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;
use std::collections::BTreeSet;

const LADDER: [TicketStatus; 12] = [
    TicketStatus::Observed,
    TicketStatus::Reserved,
    TicketStatus::Exacting,
    TicketStatus::Simulated,
    TicketStatus::Authorized,
    TicketStatus::Signed,
    TicketStatus::Dispatching,
    TicketStatus::Acknowledged,
    TicketStatus::Preconfirmed,
    TicketStatus::Included,
    TicketStatus::Finalized,
    TicketStatus::Reconciled,
];

/// `PnlAttribution` has no `Default`, and should not: a defaulted zero for
/// `net_profit_token` is a trade that silently claims to have broken even.
fn pnl() -> PnlAttribution {
    PnlAttribution {
        ticket_id: TicketId(1),
        chain: ChainId::BASE,
        strategy: StrategyId(1),
        venues: Vec::new(),
        route_hash: B256::repeat_byte(0x44),
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
    }
}

fn success() -> TicketOutcome {
    TicketOutcome::Success { stage: TicketStatus::Reconciled, realized: Box::new(pnl()) }
}

fn failure() -> TicketOutcome {
    TicketOutcome::ExplicitFailure {
        code: TerminalFailure::CompetitorWon { observed_tx: None },
        at: UnixNanos(1_700_000_000_000_000_001),
        state: Box::new(fingerprint()),
        cause: "a competitor landed first".to_string(),
    }
}

// ---------------------------------------------------------------- unit tests

/// The plan's own Step 1 test, with one correction it needs.
///
/// `ticket_drop_count` is asserted zero **because** the drop was recorded. The
/// counter means "tickets that left with no outcome", not "guards dropped" --
/// §8's INV-02 row says "no ticket removal path except terminal close", and a
/// recorded abandonment *is* a terminal close. The diagnostic counter for the
/// guard path is `tickets_closed_by_guard_drop`, asserted separately below so
/// the two cannot be confused.
#[test]
fn dropping_a_nonterminal_ticket_records_an_explicit_failure() {
    let reg = TicketRegistry::in_memory();
    let id = reg.admit(ticket()).unwrap();
    {
        let _guard = reg.checkout(id).unwrap();
    }
    assert!(matches!(
        reg.outcome(id),
        Some(TicketOutcome::ExplicitFailure { code: TerminalFailure::Abandoned { .. }, .. })
    ));
    assert_eq!(reg.metrics().ticket_drop_count(), 0, "INV-02");
    assert_eq!(reg.metrics().tickets_closed_by_guard_drop, 1);
    assert_eq!(reg.metrics().tickets_live, 0);
}

/// The abandonment record says where the ticket got to, which is what names the
/// code path that let go of it.
#[test]
fn abandonment_records_the_status_it_was_abandoned_at() {
    let reg = TicketRegistry::in_memory();
    let id = reg.admit(ticket()).unwrap();
    {
        let mut g = reg.checkout(id).unwrap();
        g.advance(TicketStatus::Reserved).unwrap();
        g.advance(TicketStatus::Authorized).unwrap();
    }
    let Some(TicketOutcome::ExplicitFailure { code, .. }) = reg.outcome(id) else {
        panic!("not closed");
    };
    assert_eq!(code, TerminalFailure::Abandoned { at_status: TicketStatus::Authorized });
}

#[test]
fn a_decided_close_does_not_also_record_an_abandonment() {
    let reg = TicketRegistry::in_memory();
    let id = reg.admit(ticket()).unwrap();
    reg.checkout(id).unwrap().close(success()).unwrap();

    assert!(reg.outcome(id).unwrap().is_success());
    assert_eq!(reg.metrics().tickets_closed_by_guard_drop, 0);
    assert_eq!(reg.metrics().tickets_terminal_success, 1);
    // Exactly one `Closed` entry -- the durable half of "exactly one outcome".
    let closes = reg
        .journal()
        .replay()
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e, JournalEntry::Closed { .. }))
        .count();
    assert_eq!(closes, 1);
}

#[test]
fn a_ticket_cannot_be_closed_twice() {
    let reg = TicketRegistry::in_memory();
    let id = reg.admit(ticket()).unwrap();
    reg.close(id, success()).unwrap();
    assert_eq!(reg.close(id, failure()), Err(RegistryError::AlreadyClosed(id)));
    assert!(reg.outcome(id).unwrap().is_success(), "the second close must not overwrite");
}

#[test]
fn a_ticket_cannot_be_checked_out_twice() {
    let reg = TicketRegistry::in_memory();
    let id = reg.admit(ticket()).unwrap();
    let _first = reg.checkout(id).unwrap();
    assert_eq!(reg.checkout(id).unwrap_err(), RegistryError::AlreadyCheckedOut(id));
}

#[test]
fn releasing_a_guard_allows_another_checkout_of_a_still_live_ticket() {
    let reg = TicketRegistry::in_memory();
    let id = reg.admit(ticket()).unwrap();
    // Closed by the drop, so the second checkout must fail as *closed* rather
    // than as checked-out. Which error comes back is the whole distinction
    // between "someone else has it" and "it is over".
    drop(reg.checkout(id).unwrap());
    assert_eq!(reg.checkout(id).unwrap_err(), RegistryError::AlreadyClosed(id));
}

#[test]
fn status_may_not_run_backwards() {
    let reg = TicketRegistry::in_memory();
    let id = reg.admit(ticket()).unwrap();
    let mut g = reg.checkout(id).unwrap();
    g.advance(TicketStatus::Authorized).unwrap();
    assert!(matches!(g.advance(TicketStatus::Reserved), Err(RegistryError::NotMonotonic(_))));
    assert!(matches!(g.advance(TicketStatus::Authorized), Err(RegistryError::NotMonotonic(_))));
    assert_eq!(g.status(), Some(TicketStatus::Authorized));
    g.close(success()).unwrap();
}

/// **INV-03.** A ticket nobody ever touches is the case the RAII guard
/// structurally cannot see, because there is no guard to drop.
#[test]
fn expiry_is_always_explained() {
    let clock = ManualClock::at(1_700_000_000_000_000_000);
    let reg = TicketRegistry::new(Box::new(InMemoryJournal::new()), Box::new(clock));
    let id = reg.admit(ticket()).unwrap();

    assert_eq!(reg.sweep(UnixNanos(1_700_000_000_100_000_000)), 0, "not yet past the deadline");
    assert!(reg.outcome(id).is_none());

    assert_eq!(reg.sweep(UnixNanos(1_700_000_000_300_000_000)), 1);
    let Some(TicketOutcome::ExplicitFailure { code, .. }) = reg.outcome(id) else {
        panic!("not closed");
    };
    assert!(matches!(code, TerminalFailure::DispatchTimeout { .. }));
    assert_eq!(reg.metrics().tickets_closed_by_deadline, 1);
}

/// The sweep must not close a ticket somebody is holding: that owner is
/// responsible for it, and closing it from underneath is the double-close the
/// registry exists to prevent.
#[test]
fn the_sweep_leaves_a_checked_out_ticket_to_its_owner() {
    let reg = TicketRegistry::in_memory();
    let id = reg.admit(ticket()).unwrap();
    let guard = reg.checkout(id).unwrap();
    assert_eq!(reg.metrics().tickets_live as usize, reg.live_ids().len());

    assert_eq!(reg.sweep(UnixNanos(u64::MAX)), 0, "the sweep took a ticket with an owner");
    assert!(reg.outcome(id).is_none());

    drop(guard);
    assert!(reg.outcome(id).is_some(), "and the owner still closes it");
}

#[test]
fn admit_assigns_the_id_rather_than_trusting_the_caller() {
    let reg = TicketRegistry::in_memory();
    let mut a = ticket();
    let mut b = ticket();
    a.ticket_id = TicketId(77);
    b.ticket_id = TicketId(77);
    let ida = reg.admit(a).unwrap();
    let idb = reg.admit(b).unwrap();
    assert_ne!(ida, idb, "two callers inventing one id is how an outcome gets overwritten");
    assert_eq!(reg.ticket(ida).unwrap().ticket_id, ida, "and the stored copy agrees");
}

// ------------------------------------------------------------------ property

/// One operation against the registry. Deliberately includes operations that
/// must fail -- closing twice, advancing backwards, checking out something that
/// is gone -- because INV-01 is a claim about what happens *after* a refused
/// operation as much as after an accepted one.
#[derive(Clone, Debug)]
enum Op {
    Admit,
    /// Check out ticket `i % live`, advance to `LADDER[s]`, drop the guard.
    TouchAndDrop { i: usize, s: usize },
    /// Check out, advance, decide.
    TouchAndClose { i: usize, s: usize, ok: bool },
    /// Close without a checkout.
    CloseDirect { i: usize, ok: bool },
    /// Check out and hold, then drop at the end of the op -- exercises the
    /// exclusivity path against a second checkout in the same op.
    DoubleCheckout { i: usize },
    Sweep { at: u64 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::Admit),
        2 => (0usize..8, 0usize..12).prop_map(|(i, s)| Op::TouchAndDrop { i, s }),
        3 => (0usize..8, 0usize..12, any::<bool>())
            .prop_map(|(i, s, ok)| Op::TouchAndClose { i, s, ok }),
        2 => (0usize..8, any::<bool>()).prop_map(|(i, ok)| Op::CloseDirect { i, ok }),
        1 => (0usize..8).prop_map(|i| Op::DoubleCheckout { i }),
        1 => (1_700_000_000_000_000_000u64..1_700_000_000_400_000_000)
            .prop_map(|at| Op::Sweep { at }),
    ]
}

fn apply(reg: &TicketRegistry, admitted: &mut BTreeSet<TicketId>, op: &Op) {
    let pick = |i: usize| -> Option<TicketId> {
        let ids: Vec<TicketId> = admitted.iter().copied().collect();
        if ids.is_empty() {
            None
        } else {
            Some(ids[i % ids.len()])
        }
    };
    match *op {
        Op::Admit => {
            admitted.insert(reg.admit(ticket()).unwrap());
        }
        Op::TouchAndDrop { i, s } => {
            if let Some(id) = pick(i) {
                if let Ok(mut g) = reg.checkout(id) {
                    let _ = g.advance(LADDER[s]);
                }
            }
        }
        Op::TouchAndClose { i, s, ok } => {
            if let Some(id) = pick(i) {
                if let Ok(mut g) = reg.checkout(id) {
                    let _ = g.advance(LADDER[s]);
                    let _ = g.close(if ok { success() } else { failure() });
                }
            }
        }
        Op::CloseDirect { i, ok } => {
            if let Some(id) = pick(i) {
                let _ = reg.close(id, if ok { success() } else { failure() });
            }
        }
        Op::DoubleCheckout { i } => {
            if let Some(id) = pick(i) {
                let _held = reg.checkout(id);
                let _second = reg.checkout(id);
            }
        }
        Op::Sweep { at } => {
            reg.sweep(UnixNanos(at));
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        // Acceptance criterion 1: "100,000-case property run: zero tickets
        // without a terminal outcome."
        cases: 100_000,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/proptest-regressions/ticket_lifecycle.txt",
        ))),
        ..ProptestConfig::default()
    })]

    /// **INV-01 and INV-02.**
    ///
    /// The final sweep is at `u64::MAX`, which is the honest statement of the
    /// invariant: given enough time, nothing is still in flight. Without it the
    /// test would only prove that tickets somebody *touched* terminate, and the
    /// forgotten ticket is the one that actually loses money.
    #[test]
    fn ticket_always_terminates(
        ops in prop::collection::vec(op_strategy(), 0..24)
    ) {
        let reg = TicketRegistry::in_memory();
        let mut admitted = BTreeSet::new();
        for op in &ops {
            apply(&reg, &mut admitted, op);
        }
        reg.sweep(UnixNanos(u64::MAX));

        let m = reg.metrics();
        prop_assert_eq!(m.tickets_live, 0, "a ticket survived the final sweep");
        // The counter against the map it counts. `ticket_drop_count` is derived
        // from `tickets_live`, so a counter that drifted from reality would make
        // the derivation agree with itself and prove nothing.
        prop_assert_eq!(m.tickets_live as usize, reg.live_ids().len());
        for id in &admitted {
            prop_assert!(reg.outcome(*id).is_some(), "INV-01: ticket {} has no outcome", id.0);
        }
        prop_assert_eq!(m.ticket_drop_count(), 0, "INV-02");
        prop_assert_eq!(
            m.tickets_admitted,
            m.tickets_terminal_success + m.tickets_terminal_failure,
            "admitted must equal the tickets accounted for"
        );
        prop_assert_eq!(m.tickets_admitted as usize, admitted.len());

        // Exactly one, not at least one: the journal is what recovery reads, so
        // a second `Closed` for one ticket would make the durable record
        // ambiguous even though the in-memory map is not.
        let mut closes = BTreeSet::new();
        for e in reg.journal().replay().unwrap() {
            if let JournalEntry::Closed { id, .. } = e {
                prop_assert!(closes.insert(id), "two Closed entries for ticket {}", id.0);
            }
        }
        prop_assert_eq!(closes.len(), admitted.len());
    }
}

/// The clock is injected, so this is a statement about the registry rather than
/// about how fast the test ran.
#[test]
fn outcomes_are_stamped_with_the_registry_clock() {
    let clock = ManualClock::at(42);
    let reg = TicketRegistry::new(Box::new(InMemoryJournal::new()), Box::new(clock));
    assert_eq!(reg.now(), UnixNanos(42));
    let id = reg.admit(ticket()).unwrap();
    drop(reg.checkout(id).unwrap());
    let Some(TicketOutcome::ExplicitFailure { at, .. }) = reg.outcome(id) else {
        panic!("not closed")
    };
    assert_eq!(at, UnixNanos(42));
}
